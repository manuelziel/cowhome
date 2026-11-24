//! Target resolution, Digital Twin flag sync, and trace helpers for `AdaptiveStart`.
//!
//! Separated from [`super::adaptive_start`] so the `DIN EN 12098-1` pre-heating
//! window latch and its diagnostic/telemetry helpers each stay concise (`<= 420` lines).

use std::collections::HashMap;

use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::core::domain_constants::{
    automation::automation_defaults::{PREHEAT_MAX_MINUTES, PREHEAT_MIN_DELTA_C},
    hardware::device_types,
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::models::automation::AdaptiveStartConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::weather::WeatherService;

// =============================================================================
/// Resolves target thermostat [`DeviceState`] instances from `config`.
#[must_use]
pub(super) fn resolve_target_devices(
    config: &AdaptiveStartConfig,
    registry: &StateRegistry,
) -> Vec<DeviceState> {
    let ains: Vec<&str> = if !config.target_ains.is_empty() {
        config.target_ains.iter().map(String::as_str).collect()
    } else if !config.target_ain.trim().is_empty() {
        vec![config.target_ain.as_str()]
    } else {
        Vec::new()
    };
    ains.into_iter()
        .filter_map(|a| registry.get_device(a))
        .filter(|d| {
            matches!(
                d.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            )
        })
        .collect()
}

// =============================================================================
/// Resolves minimum positive indoor temperature (`°C`) across online (or all) scope thermostats.
#[must_use]
pub(super) fn resolve_current_room_temp(devices: &[DeviceState]) -> Option<f64> {
    let online: Vec<f64> = devices
        .iter()
        .filter(|d| d.present)
        .filter_map(|d| d.as_thermostat().map(|t| t.temperature_current))
        .filter(|&t| t > 0.0)
        .collect();
    if let Some(min_on) = online.into_iter().reduce(f64::min) {
        return Some(min_on);
    }
    devices
        .iter()
        .filter_map(|d| d.as_thermostat().map(|t| t.temperature_current))
        .filter(|&t| t > 0.0)
        .reduce(f64::min)
}

// =============================================================================
/// Updates live `preheat_*` telemetry fields on `ain` in `StateRegistry`.
pub(super) fn set_twin_preheat_state(
    registry: &StateRegistry,
    ain: &str,
    active: bool,
    target_temp: Option<f64>,
    target_time: Option<&str>,
    clear_paused_reason: bool,
) {
    registry.mutate_device(ain, |dev| {
        if let Some(t) = dev.as_thermostat_mut() {
            t.preheat_active = active;
            t.preheat_target_temp = if active {
                target_temp.map(|v| round_decimals(v, 1))
            } else {
                None
            };
            t.preheat_target_time = if active {
                target_time.map(str::to_owned)
            } else {
                None
            };
            if active || clear_paused_reason {
                t.preheat_paused_reason = None;
            }
        }
    });
}

// =============================================================================
/// Builds the `AdaptiveStart` per-scope diagnostic decision trace JSON object.
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_adaptive_scope_trace(
    config: &AdaptiveStartConfig,
    is_active: bool,
    registry: &StateRegistry,
    db: &Database,
    schedule_transition: Option<(f64, f64)>,
    now_dt: DateTime<Tz>,
    weather_service: Option<&WeatherService>,
    preheating_ains: Vec<String>,
) -> Value {
    let devices = resolve_target_devices(config, registry);
    let cur_temp = resolve_current_room_temp(&devices);
    let ains: Vec<String> = devices.iter().map(|d| d.ain.clone()).collect();
    let live_out = weather_service
        .and_then(WeatherService::get_current_weather)
        .and_then(|c| c.temperature);
    let metrics = db
        .run_blocking(move |d| Ok(d.resolve_scope_heating_metrics(&ains, live_out, None, None)))
        .await
        .unwrap_or_else(|_| db.resolve_scope_heating_metrics(&[], live_out, None, None));

    let delta_t = match (cur_temp, schedule_transition) {
        (Some(ct), Some((_, tt))) => round_decimals((tt - ct).max(0.0), 2),
        _ => 0.0,
    };
    let above_db = schedule_transition.is_some() && delta_t >= PREHEAT_MIN_DELTA_C;
    #[allow(clippy::cast_possible_truncation)]
    let preheat_m = if above_db {
        ((delta_t * metrics.effective_heating_rate_min_per_c).round() as i64)
            .clamp(0, PREHEAT_MAX_MINUTES)
    } else {
        0
    };
    let paused_map: HashMap<String, String> = devices
        .iter()
        .filter_map(|d| {
            let r = d.as_thermostat()?.preheat_paused_reason.clone()?;
            Some((normalize_ain(&d.ain), r))
        })
        .collect();
    let status = if !preheating_ains.is_empty() {
        "preheating"
    } else if is_active && !paused_map.is_empty() {
        "paused"
    } else if is_active && above_db {
        "waiting"
    } else {
        "idle"
    };
    #[allow(clippy::cast_possible_truncation)]
    let t_str = schedule_transition.and_then(|(ep, _)| {
        now_dt
            .timezone()
            .timestamp_opt(ep as i64, 0)
            .single()
            .map(|dt| dt.format("%H:%M").to_string())
    });
    json!({
        "is_active": is_active,
        "preheat_enabled": is_active,
        "status": status,
        "target_time": t_str,
        "target_temp": schedule_transition.map(|(_, tt)| tt),
        "current_temp": cur_temp,
        "delta_temp": delta_t,
        "heating_rate_min_per_c": metrics.heating_rate_min_per_c,
        "effective_heating_rate_min_per_c": metrics.effective_heating_rate_min_per_c,
        "heating_rate_c_per_hour": metrics.heating_rate_c_per_hour,
        "outdoor_temp_c": metrics.outdoor_temp_c,
        "outdoor_inertia_factor": metrics.outdoor_inertia_factor,
        "preheat_minutes": preheat_m,
        "preheating_ains": preheating_ains,
        "paused_reasons": paused_map,
    })
}
