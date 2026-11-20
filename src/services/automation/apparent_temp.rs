//! `ApparentTemperature` (`Room Sensor Sync`) & standalone static calibration (`set_offset`).
//!
//! Links external `FRITZ!Smart Control 440` room climate sensors as master reference
//! sensors for radiator thermostats, applies dynamic target offset compensation with
//! overheat protection, and enforces standalone static calibration (`temperature_offset`).

use serde_json::{json, Value};

use super::actuation::{OFFSET_HYSTERESIS_STEP, RULE_TEMP_EPSILON};
use crate::core::domain_constants::{
    automation::sync_defaults::TARGET_HOLD_SECONDS,
    hardware::{
        device_types,
        fritz_api::{HALF_DEGREE_FACTOR, MAX_THERMOSTAT_TEMP, MIN_THERMOSTAT_TEMP},
    },
};
use crate::core::logger::Logger;
use crate::core::psychrometrics::{
    calculate_apparent_temperature, calculate_static_offset_target, round_decimals,
};
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::models::automation::ApparentTemperatureConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;

// =============================================================================
/// Master-slave room climate sensor sync & valve offset compensation rule.
#[derive(Debug)]
pub struct ApparentTemperatureRule {
    /// Application logger.
    logger: Logger,
}

impl ApparentTemperatureRule {
    /// Creates a new [`ApparentTemperatureRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self { logger }
    }

    /// Enriches `StateRegistry` thermostats with reference sensor readings before predictive rules run.
    pub fn enrich_only(&self, config: &ApparentTemperatureConfig, registry: &StateRegistry) {
        if config.sensor_mapping.is_empty() {
            return;
        }
        for (thermo_ain, sensor_ain) in &config.sensor_mapping {
            let Some(thermostat) = registry.get_device(thermo_ain) else {
                continue;
            };
            if !is_thermostat_type(&thermostat.device_type) {
                continue;
            }
            let sensor = if sensor_ain.trim().is_empty() {
                Some(thermostat.clone())
            } else {
                registry.get_device(sensor_ain)
            };
            let Some(sensor_dev) = sensor else {
                continue;
            };
            let is_external = normalize_ain(&sensor_dev.ain) != normalize_ain(&thermostat.ain);
            if is_external && !sensor_dev.present {
                registry.mutate_device(&thermostat.ain, |d| {
                    if let Some(t) = d.as_thermostat_mut() {
                        if let Some(hw) = t.temperature_hardware.filter(|&v| v > 0.0) {
                            t.temperature_current = hw;
                        }
                        t.offset_control_active = false;
                    }
                });
                continue;
            }
            let valid_offset = is_external
                && config.active_offset_control
                && has_valid_offset_telemetry(&thermostat, &sensor_dev);
            apply_sensor_to_thermostat(registry, &thermostat.ain, &sensor_dev, valid_offset);
        }
    }

    /// Evaluates `sensor_mapping` and dispatches compensated valve targets (`Step 5`).
    pub async fn evaluate(
        &self,
        config: &ApparentTemperatureConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        if config.sensor_mapping.is_empty() {
            return;
        }
        for (thermo_ain, sensor_ain) in &config.sensor_mapping {
            let Some(thermostat) = registry.get_device(thermo_ain) else {
                continue;
            };
            if !is_thermostat_type(&thermostat.device_type) {
                continue;
            }
            let sensor = if sensor_ain.trim().is_empty() {
                Some(thermostat.clone())
            } else {
                registry.get_device(sensor_ain)
            };
            let Some(sensor_dev) = sensor else {
                continue;
            };
            let is_external = normalize_ain(&sensor_dev.ain) != normalize_ain(&thermostat.ain);
            if is_external && !sensor_dev.present {
                registry.mutate_device(&thermostat.ain, |d| {
                    if let Some(t) = d.as_thermostat_mut() {
                        if let Some(hw) = t.temperature_hardware.filter(|&v| v > 0.0) {
                            t.temperature_current = hw;
                        }
                    }
                });
                self.restore_uncompensated_target(&thermostat, registry, aha)
                    .await;
                continue;
            }
            let valid_offset = is_external
                && config.active_offset_control
                && has_valid_offset_telemetry(&thermostat, &sensor_dev);
            apply_sensor_to_thermostat(registry, &thermostat.ain, &sensor_dev, valid_offset);
            if !is_external {
                continue;
            }
            if let Some(refreshed) = registry.get_device(&thermostat.ain) {
                if config.active_offset_control {
                    self.apply_dynamic_offset(
                        &refreshed,
                        &sensor_dev,
                        config.max_offset,
                        registry,
                        aha,
                    )
                    .await;
                } else {
                    self.restore_uncompensated_target(&refreshed, registry, aha)
                        .await;
                }
            }
        }
    }

    /// Applies CowHome's standalone `temperature_offset` (or restores `user_target`) on unlinked valves.
    pub async fn apply_static_calibration(
        &self,
        device: &DeviceState,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        let Some(t) = device.as_thermostat() else {
            return;
        };
        let eligible =
            !device.automation_locked && t.fritz_temperature_offset.abs() < RULE_TEMP_EPSILON;
        let eff_off = if eligible { t.temperature_offset } else { 0.0 };
        if let Some(hw_t) = t.temperature_hardware.filter(|&v| v > 0.0) {
            let cur_t = round_decimals(hw_t + eff_off, 1);
            let app_t = device
                .humidity
                .filter(|&rh| cur_t > 0.0 && rh > 0.0)
                .map(|rh| calculate_apparent_temperature(cur_t, rh));
            registry.mutate_device(&device.ain, |d| {
                if let Some(tm) = d.as_thermostat_mut() {
                    tm.temperature_current = cur_t;
                    if app_t.is_some() {
                        tm.apparent_temperature = app_t;
                    }
                }
            });
        }
        if eff_off.abs() < RULE_TEMP_EPSILON {
            self.restore_uncompensated_target(device, registry, aha)
                .await;
            return;
        }
        let user_t = t.temperature_target;
        if !(user_t > MIN_THERMOSTAT_TEMP && user_t <= MAX_THERMOSTAT_TEMP) {
            self.restore_uncompensated_target(device, registry, aha)
                .await;
            return;
        }
        if !device.present || t.window_open || t.boost_active {
            return;
        }
        let desired_hw = calculate_static_offset_target(user_t, eff_off);
        let cur_hw = t.hardware_target_temperature.unwrap_or(user_t);
        if (desired_hw - cur_hw).abs() >= (OFFSET_HYSTERESIS_STEP - RULE_TEMP_EPSILON) {
            self.dispatch_valve_and_sync(&device.ain, &device.name, desired_hw, registry, aha)
                .await;
        }
    }

    /// Restores `hardware_target_temperature = temperature_target` when offset control is disabled.
    pub async fn restore_uncompensated_target(
        &self,
        thermostat: &DeviceState,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        let Some(t) = thermostat.as_thermostat() else {
            return;
        };
        if !thermostat.present || t.window_open || t.boost_active {
            return;
        }
        let user_t = t.temperature_target;
        let Some(hw_t) = t.hardware_target_temperature else {
            return;
        };
        if user_t > 0.0 && (hw_t - user_t).abs() >= (OFFSET_HYSTERESIS_STEP - RULE_TEMP_EPSILON) {
            self.logger.info(&format!(
                "Restoring uncompensated target {user_t}°C on {}.",
                thermostat.name
            ));
            self.dispatch_valve_and_sync(&thermostat.ain, &thermostat.name, user_t, registry, aha)
                .await;
        }
    }

    /// Builds the `ApparentTemperature` diagnostic decision trace JSON object.
    #[must_use]
    pub fn describe_scope_trace(
        &self,
        config: &ApparentTemperatureConfig,
        is_active: bool,
        targets: &[String],
        registry: &StateRegistry,
    ) -> Value {
        let mut dev_map = serde_json::Map::new();
        for ain in targets {
            let Some(dev) = registry.get_device(ain) else {
                continue;
            };
            let Some(t) = dev.as_thermostat() else {
                continue;
            };
            let offset = t
                .temperature_hardware
                .map(|hw| round_decimals(hw - t.temperature_current, 2));
            let overheat = t.temperature_current >= t.temperature_target;
            dev_map.insert(
                dev.ain.clone(),
                json!({
                    "reference_sensor_ain": t.reference_sensor_ain,
                    "sensor_offset": offset,
                    "overheat_protection_active": overheat,
                    "hardware_target_temperature": t.hardware_target_temperature,
                }),
            );
        }
        json!({
            "is_active": is_active,
            "active_offset_control": config.active_offset_control,
            "devices": dev_map,
        })
    }

    /// Computes and dispatches dynamic room-sensor valve compensation with overheat protection.
    async fn apply_dynamic_offset(
        &self,
        thermostat: &DeviceState,
        sensor: &DeviceState,
        max_offset: f64,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        if !has_valid_offset_telemetry(thermostat, sensor) {
            return;
        }
        let Some(t) = thermostat.as_thermostat() else {
            return;
        };
        let user_t = t.temperature_target;
        if !(user_t > MIN_THERMOSTAT_TEMP && user_t <= MAX_THERMOSTAT_TEMP) {
            return;
        }
        let Some(hw_temp) = t.temperature_hardware else {
            return;
        };
        let Some(room_temp) = sensor_temperature(sensor) else {
            return;
        };
        let desired_hw = calculate_compensated_target(user_t, hw_temp, room_temp, max_offset);
        let cur_hw = t.hardware_target_temperature.unwrap_or(user_t);
        if (desired_hw - cur_hw).abs() >= (OFFSET_HYSTERESIS_STEP - RULE_TEMP_EPSILON) {
            self.logger.info(&format!(
                "Offset Control [{}]: Room {room_temp}°C vs HKR {hw_temp}°C \
                 (Target {user_t}°C) -> Valve {desired_hw}°C",
                thermostat.name
            ));
            self.dispatch_valve_and_sync(
                &thermostat.ain,
                &thermostat.name,
                desired_hw,
                registry,
                aha,
            )
            .await;
        }
    }

    /// Sends `desired_hw` to AHA, updating `hardware_target_temperature` or logging gateway errors.
    async fn dispatch_valve_and_sync(
        &self,
        ain: &str,
        name: &str,
        desired_hw: f64,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        match aha.set_target_temperature(ain, desired_hw).await {
            Ok(()) => update_hw_valve_target(registry, ain, desired_hw),
            Err(err) => self.logger.warning(&format!(
                "Offset Control failed setting valve target {desired_hw}°C on '{name}' ({ain}): {err}"
            )),
        }
    }
}

// =============================================================================
/// Calculates `0.5 °C`-quantized hardware valve target with overheat protection.
#[must_use]
pub fn calculate_compensated_target(
    user_target: f64,
    hw_temp: f64,
    room_temp: f64,
    max_offset: f64,
) -> f64 {
    let limit = max_offset.max(0.0);
    let raw_offset = if room_temp < user_target {
        hw_temp - room_temp
    } else {
        (hw_temp - room_temp).min(user_target - room_temp)
    };
    let clamped = raw_offset.clamp(-limit, limit);
    let half_step = ((user_target + clamped) * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR;
    half_step.clamp(MIN_THERMOSTAT_TEMP, MAX_THERMOSTAT_TEMP)
}

// =============================================================================
/// Copies `sensor` temperature/humidity/apparent-temp onto `thermostat_ain` in `StateRegistry`.
fn apply_sensor_to_thermostat(
    registry: &StateRegistry,
    thermostat_ain: &str,
    sensor: &DeviceState,
    offset_control_active: bool,
) {
    let s_temp = sensor_temperature(sensor).filter(|&v| v > 0.0);
    let s_hum = sensor.humidity;
    let s_ain = sensor.ain.clone();
    registry.mutate_device(thermostat_ain, |d| {
        if let Some(rh) = s_hum {
            d.humidity = Some(rh);
        }
        let eff_rh = d.humidity;
        let is_ext = d.ain != s_ain;
        if let Some(t) = d.as_thermostat_mut() {
            if is_ext {
                t.reference_sensor_ain = Some(s_ain);
                t.offset_control_active = offset_control_active;
            }
            if let Some(st) = s_temp {
                t.temperature_current = st;
            }
            if let Some(rh) = eff_rh.filter(|&h| t.temperature_current > 0.0 && h > 0.0) {
                t.apparent_temperature =
                    Some(calculate_apparent_temperature(t.temperature_current, rh));
            }
        }
    });
}

// =============================================================================
/// Updates `hardware_target_temperature` and arms `target_hold_until` on `ain`.
fn update_hw_valve_target(registry: &StateRegistry, ain: &str, desired_hw: f64) {
    let hold = now_epoch_f64() + TARGET_HOLD_SECONDS;
    registry.mutate_device(ain, |d| {
        if let Some(t) = d.as_thermostat_mut() {
            t.hardware_target_temperature = Some(desired_hw);
            t.target_hold_until = hold;
        }
    });
}

// =============================================================================
/// Returns `true` when both thermostat and reference sensor report positive temperatures.
fn has_valid_offset_telemetry(thermostat: &DeviceState, sensor: &DeviceState) -> bool {
    let Some(t) = thermostat.as_thermostat() else {
        return false;
    };
    if !thermostat.present || !sensor.present || t.window_open || t.boost_active {
        return false;
    }
    t.temperature_hardware.is_some_and(|hw| hw > 0.0)
        && sensor_temperature(sensor).is_some_and(|rt| rt > 0.0)
}

// =============================================================================
/// Reads measured temperature (`°C`) from a sensor or thermostat `DeviceState`.
fn sensor_temperature(sensor: &DeviceState) -> Option<f64> {
    sensor
        .as_sensor()
        .and_then(|s| s.temperature_current)
        .or_else(|| sensor.as_thermostat().map(|t| t.temperature_current))
}

// =============================================================================
/// Checks whether `device_type` is `"thermostat"` or `"thermostat_group"`.
fn is_thermostat_type(device_type: &str) -> bool {
    matches!(
        device_type,
        device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
    )
}
