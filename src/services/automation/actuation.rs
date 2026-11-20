//! Shared thermostat valve dispatch, hold-timer arming, & local-time helpers for `RulesEngine`.
//!
//! Centralizes DECT-ULE `90 s` target hold updates, dynamic `Offset Control` valve
//! deferral (`offset_control_active`), and home IANA timezone (`chrono-tz`) wall-clock
//! lookups across all 7 automation features.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

pub use crate::core::domain_constants::automation::{
    humidity_guard_states, preheat_paused_reasons,
};
use crate::core::domain_constants::{
    automation::sync_defaults::TARGET_HOLD_SECONDS,
    hardware::fritz_api::{MAX_THERMOSTAT_TEMP, MIN_THERMOSTAT_TEMP},
};
use crate::core::error::AppResult;
use crate::core::time_util::{now_epoch_f64, resolve_home_tz};
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;
use crate::services::weather::WeatherService;

/// Floating-point threshold (`0.01 °C`) for setpoint comparisons inside `RulesEngine`.
pub const RULE_TEMP_EPSILON: f64 = 0.01;

/// Hysteresis step (`0.5 °C`) for dynamic and static valve offset updates.
pub const OFFSET_HYSTERESIS_STEP: f64 = 0.5;

// =============================================================================
/// Returns the current timezone-aware [`DateTime<Tz>`] in the home location's IANA timezone.
#[must_use]
pub fn now_local_dt(weather_service: Option<&WeatherService>, fallback_tz: &str) -> DateTime<Tz> {
    let tz = weather_service.map_or_else(
        || resolve_home_tz(Some(fallback_tz)),
        WeatherService::get_tz,
    );
    Utc::now().with_timezone(&tz)
}

// =============================================================================
/// Updates `temperature_target`, `hardware_target_temperature`, and `target_hold_until` in RAM.
pub fn apply_twin_thermostat_target(registry: &StateRegistry, ain: &str, target_temp: f64) {
    let hold_until = now_epoch_f64() + TARGET_HOLD_SECONDS;
    registry.mutate_device(ain, |dev| {
        if let Some(t) = dev.as_thermostat_mut() {
            t.temperature_target = target_temp;
            t.hardware_target_temperature = Some(target_temp);
            t.target_hold_until = hold_until;
        }
    });
}

// =============================================================================
/// Updates `temperature_target` & `target_hold_until` while preserving `hardware_target_temperature`.
pub fn defer_twin_thermostat_target(registry: &StateRegistry, ain: &str, target_temp: f64) {
    let hold_until = now_epoch_f64() + TARGET_HOLD_SECONDS;
    registry.mutate_device(ain, |dev| {
        if let Some(t) = dev.as_thermostat_mut() {
            if t.hardware_target_temperature.is_none() {
                t.hardware_target_temperature = Some(t.temperature_target);
            }
            t.temperature_target = target_temp;
            t.target_hold_until = hold_until;
        }
    });
}

// =============================================================================
/// Checks live telemetry to ensure Step 5 (`Offset Control`) will actively handle `device`.
#[must_use]
pub fn can_defer_valve_to_offset_step(
    device: &DeviceState,
    target_temp: f64,
    registry: &StateRegistry,
) -> bool {
    if !(target_temp > MIN_THERMOSTAT_TEMP && target_temp <= MAX_THERMOSTAT_TEMP) || !device.present
    {
        return false;
    }
    let Some(t) = device.as_thermostat() else {
        return false;
    };
    if !t.offset_control_active
        || t.window_open
        || t.boost_active
        || !t.temperature_hardware.is_some_and(|hw| hw > 0.0)
    {
        return false;
    }
    if let Some(ref sensor_ain) = t.reference_sensor_ain {
        return registry.get_device(sensor_ain).is_some_and(|sensor| {
            sensor.present
                && sensor
                    .as_sensor()
                    .and_then(|s| s.temperature_current)
                    .or_else(|| sensor.as_thermostat().map(|st| st.temperature_current))
                    .is_some_and(|rt| rt > 0.0)
        });
    }
    !device.automation_locked
        && t.fritz_temperature_offset.abs() < RULE_TEMP_EPSILON
        && t.temperature_offset.abs() >= RULE_TEMP_EPSILON
}

// =============================================================================
/// Dispatches `target_temp` via AHA or defers hardware valve actuation to Step 5 Offset Control.
///
/// # Errors
/// Returns [`crate::core::error::AppError`] if `aha.set_target_temperature` fails.
pub async fn dispatch_thermostat_target(
    device: &DeviceState,
    target_temp: f64,
    registry: &StateRegistry,
    aha: &FritzAhaClient,
) -> AppResult<()> {
    if can_defer_valve_to_offset_step(device, target_temp, registry) {
        defer_twin_thermostat_target(registry, &device.ain, target_temp);
        return Ok(());
    }
    aha.set_target_temperature(&device.ain, target_temp).await?;
    apply_twin_thermostat_target(registry, &device.ain, target_temp);
    Ok(())
}
