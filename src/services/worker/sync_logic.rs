//! Dial-turn target resolution, DECT-ULE hold preservation, & room-sensor enrichment.
//!
//! Preserves user setpoints (`temperature_target`), compensated hardware valve commands
//! (`hardware_target_temperature`), `90 s` DECT-ULE hold windows, `RulesEngine` flags,
//! linked `FRITZ!Smart Control 440` readings, and `DIN 4108-2` psychrometric telemetry
//! across polling cycles and daemon restarts.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::state_builder::TEMP_CHANGE_THRESHOLD;
use crate::core::domain_constants::{
    automation::automation_defaults::{HUMIDITY_HYSTERESIS_PERCENT, HUMIDITY_THRESHOLD},
    hardware::device_types,
};
use crate::core::psychrometrics::{
    calculate_apparent_temperature, compute_device_psychrometrics, DevicePsychrometricsInput,
};
use crate::core::time_util::normalize_ain;
use crate::models::device::{DeviceState, ThermostatFields};
use crate::models::registry::StateRegistry;
use crate::services::weather::WeatherService;

// =============================================================================
/// Resolves user target temperature (`°C`), ignoring offset compensation & DECT-ULE poll lag.
#[must_use]
pub fn resolve_effective_user_target(
    registry: &StateRegistry,
    ain: &str,
    reported_target: f64,
    prev_opt: Option<&DeviceState>,
    now_ts: f64,
) -> f64 {
    let owned_prev;
    let prev = match prev_opt {
        Some(p) => p,
        None => {
            owned_prev = registry.get_device(ain);
            let Some(ref p) = owned_prev else {
                return reported_target;
            };
            p
        }
    };
    let Some(t_prev) = prev.as_thermostat() else {
        return reported_target;
    };
    let user_t = t_prev.temperature_target;
    if t_prev.target_hold_until > now_ts && user_t > 0.0 {
        return user_t;
    }
    if let Some(hw_t) = t_prev.hardware_target_temperature {
        if (hw_t - user_t).abs() > TEMP_CHANGE_THRESHOLD
            && ((reported_target - hw_t).abs() <= TEMP_CHANGE_THRESHOLD
                || (reported_target - user_t).abs() <= TEMP_CHANGE_THRESHOLD)
        {
            return user_t;
        }
    }
    if let Some(prev_raw) = registry.get_raw_reported_target(ain) {
        if user_t > 0.0
            && (reported_target - user_t).abs() > TEMP_CHANGE_THRESHOLD
            && (reported_target - prev_raw).abs() <= TEMP_CHANGE_THRESHOLD
        {
            return user_t;
        }
    }
    reported_target
}

// =============================================================================
/// Preserves `RulesEngine` flags, hold timers, reference sensor readings, & psychrometrics.
pub fn preserve_enriched_twin_state(
    state: &mut DeviceState,
    registry: &StateRegistry,
    baseline_map: Option<&Map<String, Value>>,
    weather_service: Option<&WeatherService>,
    last_targets: &mut HashMap<String, f64>,
    now_ts: f64,
) {
    let raw_prev = registry.get_device(&state.ain);
    if let Some(ref prev_dev) = raw_prev {
        copy_twin_flags(prev_dev, state);
    }
    if state.device_type == device_types::THERMOSTAT {
        let synth_prev = if raw_prev.is_none() {
            baseline_map.and_then(|m| reconstruct_baseline_thermostat(state, m))
        } else {
            None
        };
        let eff_prev = raw_prev.as_ref().or(synth_prev.as_ref());
        if let Some(prev_dev) = eff_prev {
            if raw_prev.is_none() {
                copy_twin_flags(prev_dev, state);
                if let Some(t) = state.as_thermostat() {
                    last_targets.insert(normalize_ain(&state.ain), t.temperature_target);
                }
            }
            preserve_thermostat_targets(prev_dev, state, registry, now_ts);
            sync_thermostat_reference_sensor(prev_dev, state, registry);
        } else if let Some(t) = state.as_thermostat() {
            registry.record_raw_reported_target(&state.ain, t.temperature_target);
        }
    }
    enrich_polled_psychrometrics(state, raw_prev.as_ref(), weather_service);
}

// =============================================================================
/// Preserves user setpoint, compensated hardware target, and hold timer across polls.
fn preserve_thermostat_targets(
    prev: &DeviceState,
    state: &mut DeviceState,
    registry: &StateRegistry,
    now_ts: f64,
) {
    let Some(t_prev) = prev.as_thermostat() else {
        return;
    };
    let ain = state.ain.clone();
    let Some(raw_reported) = state.as_thermostat().map(|t| t.temperature_target) else {
        return;
    };
    if t_prev.target_hold_until > now_ts && t_prev.temperature_target > 0.0 {
        let exp_hw = t_prev
            .hardware_target_temperature
            .unwrap_or(t_prev.temperature_target);
        if registry.get_raw_reported_target(&ain).is_none()
            || (raw_reported - exp_hw).abs() <= TEMP_CHANGE_THRESHOLD
        {
            registry.record_raw_reported_target(&ain, raw_reported);
        }
        if let Some(t_cur) = state.as_thermostat_mut() {
            t_cur.temperature_target = t_prev.temperature_target;
            t_cur.hardware_target_temperature = t_prev.hardware_target_temperature;
            t_cur.target_hold_until = t_prev.target_hold_until;
        }
        return;
    }
    let eff_target =
        resolve_effective_user_target(registry, &ain, raw_reported, Some(prev), now_ts);
    registry.record_raw_reported_target(&ain, raw_reported);
    if let Some(t_cur) = state.as_thermostat_mut() {
        if (eff_target - raw_reported).abs() > TEMP_CHANGE_THRESHOLD {
            t_cur.temperature_target = eff_target;
            t_cur.hardware_target_temperature = t_prev.hardware_target_temperature;
        } else if let Some(prev_hw) = t_prev.hardware_target_temperature {
            if (prev_hw - t_prev.temperature_target).abs() > TEMP_CHANGE_THRESHOLD
                && (raw_reported - t_prev.temperature_target).abs() <= TEMP_CHANGE_THRESHOLD
            {
                t_cur.hardware_target_temperature = Some(prev_hw);
            }
        }
    }
}

// =============================================================================
/// Copies linked external room sensor (`DECT 440`) climate values onto `state`.
fn sync_thermostat_reference_sensor(
    prev: &DeviceState,
    state: &mut DeviceState,
    registry: &StateRegistry,
) {
    let Some(t_prev) = prev.as_thermostat() else {
        return;
    };
    if let Some(ref ref_ain) = t_prev.reference_sensor_ain {
        let ref_dev = registry.get_device(ref_ain).filter(|d| d.present);
        if let Some(t_cur) = state.as_thermostat_mut() {
            t_cur.reference_sensor_ain = Some(ref_ain.clone());
            if let Some(ref sensor) = ref_dev {
                if let Some(st) = sensor
                    .as_sensor()
                    .and_then(|s| s.temperature_current)
                    .or_else(|| sensor.as_thermostat().map(|t| t.temperature_current))
                {
                    t_cur.temperature_current = st;
                }
            } else if let Some(hw) = t_cur.temperature_hardware {
                t_cur.temperature_current = hw;
            }
        }
        if let Some(rh) = ref_dev.and_then(|d| d.humidity) {
            state.humidity = Some(rh);
        }
    } else if state.humidity.is_none() && prev.humidity.is_some() {
        state.humidity = prev.humidity;
    }
    let hum = state.humidity;
    if let Some(t_cur) = state.as_thermostat_mut() {
        if let Some(rh) = hum.filter(|&h| t_cur.temperature_current > 0.0 && h > 0.0) {
            t_cur.apparent_temperature = Some(calculate_apparent_temperature(
                t_cur.temperature_current,
                rh,
            ));
        }
    }
}

// =============================================================================
/// Enriches `DIN 4108-2` psychrometrics on `state`, preserving active `RulesEngine` advice.
fn enrich_polled_psychrometrics(
    state: &mut DeviceState,
    prev_state: Option<&DeviceState>,
    weather_service: Option<&WeatherService>,
) {
    let Some(rh_in) = state.humidity else {
        return;
    };
    let temp_in = state
        .as_thermostat()
        .map(|t| t.temperature_current)
        .or_else(|| state.as_sensor().and_then(|s| s.temperature_current))
        .or_else(|| state.as_switch().map(|sw| sw.temperature_current))
        .filter(|&t| t > 0.0);
    let fallback_target = state.as_thermostat().map(|t| t.temperature_target);
    let (temp_out, abs_out) = weather_service
        .and_then(WeatherService::get_current_weather)
        .map_or((None, None), |cw| cw.outdoor_psychrometrics());

    let mut psych = compute_device_psychrometrics(&DevicePsychrometricsInput {
        temp_in_c: temp_in,
        rel_humidity_in_pct: Some(rh_in),
        fallback_target_c: fallback_target,
        temp_base_c: None,
        temp_out_c: temp_out,
        abs_out_gm3: abs_out,
        humidity_threshold: HUMIDITY_THRESHOLD,
        hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
        guard_active: state.humidity_guard_active,
        rule_active: false,
    });
    if let Some(prev) = prev_state {
        if prev.ventilation_advice.is_some() {
            psych
                .ventilation_advice
                .clone_from(&prev.ventilation_advice);
            if state.humidity_guard_active && prev.projected_base_humidity_pct.is_some() {
                psych.projected_base_humidity_pct = prev.projected_base_humidity_pct;
            }
        }
    }
    state.apply_psychrometrics(psych);
}

// =============================================================================
/// Copies `RulesEngine` guard and `Pre-Heat` flags from `source` onto `target`.
fn copy_twin_flags(source: &DeviceState, target: &mut DeviceState) {
    target.humidity_guard_active = source.humidity_guard_active;
    if let (Some(src_t), Some(dst_t)) = (source.as_thermostat(), target.as_thermostat_mut()) {
        dst_t.preheat_active = src_t.preheat_active;
        dst_t.preheat_target_temp = src_t.preheat_target_temp;
        dst_t
            .preheat_target_time
            .clone_from(&src_t.preheat_target_time);
        dst_t
            .preheat_paused_reason
            .clone_from(&src_t.preheat_paused_reason);
        dst_t.offset_control_active = src_t.offset_control_active;
    }
}

// =============================================================================
/// Reconstructs a synthetic previous thermostat [`DeviceState`] from SQLite on initial poll.
fn reconstruct_baseline_thermostat(
    current: &DeviceState,
    baseline: &Map<String, Value>,
) -> Option<DeviceState> {
    let user_t = baseline.get("temperature_target")?.as_f64()?;
    let hw_t = baseline
        .get("hardware_target_temperature")
        .and_then(Value::as_f64)
        .unwrap_or(user_t);
    let mut dev = DeviceState::new_thermostat(
        current.ain.clone(),
        current.name.clone(),
        ThermostatFields {
            temperature_target: user_t,
            hardware_target_temperature: Some(hw_t),
            reference_sensor_ain: baseline
                .get("reference_sensor_ain")
                .and_then(Value::as_str)
                .map(str::to_owned),
            preheat_active: baseline
                .get("preheat_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            preheat_target_temp: baseline.get("preheat_target_temp").and_then(Value::as_f64),
            preheat_target_time: baseline
                .get("preheat_target_time")
                .and_then(Value::as_str)
                .map(str::to_owned),
            preheat_paused_reason: baseline
                .get("preheat_paused_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..ThermostatFields::default()
        },
    );
    dev.humidity = baseline.get("humidity").and_then(Value::as_f64);
    dev.humidity_guard_active = baseline
        .get("humidity_guard_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(dev)
}
