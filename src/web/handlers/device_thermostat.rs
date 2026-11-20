//! Thermostat setpoint, comfort/eco preset, & `set_offset` calibration executors.
//!
//! Enforces Single-Authority calibration locks (`HTTP 409 Conflict` when grouped,
//! linked to a room sensor, or running an internal FritzBox offset) and synchronizes
//! `StateRegistry` + `RulesEngine`.

use serde_json::{Map, Value};

use super::device_helpers::{parse_float_param, parse_offset_value, parse_optional_temp};
use crate::core::domain_constants::{
    automation::{
        automation_identifiers::{
            COMFORT_ACTION_KEYS, COMFORT_PARAM_KEYS, ECO_ACTION_KEYS, ECO_PARAM_KEYS, SCOPE_GROUP,
        },
        sync_defaults::{MANUAL_OVERRIDE_ACTIONS, TARGET_HOLD_SECONDS},
    },
    hardware::{
        device_actions,
        fritz_api::{MAX_THERMOSTAT_TEMP, MIN_THERMOSTAT_TEMP},
    },
};
use crate::core::error::AppError;
use crate::core::psychrometrics::{
    calculate_apparent_temperature, calculate_static_offset_target, round_decimals,
};
use crate::core::time_util::now_epoch_f64;
use crate::models::device::DeviceState;
use crate::services::automation::actuation::{
    can_defer_valve_to_offset_step, defer_twin_thermostat_target, OFFSET_HYSTERESIS_STEP,
    RULE_TEMP_EPSILON,
};
use crate::web::response::{ApiError, ApiResult};
use crate::web::state::AppState;

// =============================================================================
/// Executes `set_temperature`, `set_comfort`, or `set_eco` with offset deferral/compensation.
pub(super) async fn execute_thermostat_action(
    state: &AppState,
    ain: &str,
    action: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
    trigger_engine: bool,
) -> ApiResult<()> {
    let dev = state.registry.get_device(ain);
    if action == device_actions::SET_TEMPERATURE {
        let temp = parse_float_param(params, "temperature")?;
        dispatch_or_defer_setpoint(state, ain, dev.as_ref(), temp).await?;
    } else {
        let is_comfort = action == device_actions::SET_COMFORT;
        execute_comfort_or_eco_action(state, ain, dev.as_ref(), is_comfort, params, eff_ts).await?;
    }

    if trigger_engine {
        notify_and_evaluate_manual_action(state, &[ain.to_owned()], action).await;
    }
    Ok(())
}

// =============================================================================
/// Applies `set_comfort` or `set_eco`, deferring raw AHA calls when room-sensor offset is active.
async fn execute_comfort_or_eco_action(
    state: &AppState,
    ain: &str,
    dev: Option<&DeviceState>,
    is_comfort: bool,
    params: &Map<String, Value>,
    eff_ts: f64,
) -> ApiResult<()> {
    let keys = if is_comfort {
        COMFORT_ACTION_KEYS
    } else {
        ECO_ACTION_KEYS
    };
    let opt_t = parse_optional_temp(params, keys)?;
    let (comfort, eco) = persist_and_load_presets(state, ain, is_comfort, opt_t, eff_ts).await?;
    let target = if is_comfort { comfort } else { eco };
    sync_twin_presets(state, ain, comfort, eco, None, None).await;
    dispatch_preset_valve_target(state, ain, dev, is_comfort, target).await
}

// =============================================================================
/// Persists optional updated `comfort`/`eco` preset in SQLite and returns `(comfort, eco)`.
async fn persist_and_load_presets(
    state: &AppState,
    ain: &str,
    is_comfort: bool,
    opt_temp: Option<f64>,
    eff_ts: f64,
) -> ApiResult<(f64, f64)> {
    let ain_owned = ain.to_owned();
    state
        .db
        .run_blocking(move |db| {
            if let Some(t) = opt_temp {
                let (c, e) = if is_comfort {
                    (Some(t), None)
                } else {
                    (None, Some(t))
                };
                db.set_thermostat_presets(&ain_owned, c, e, Some(eff_ts))?;
            }
            Ok(db.get_thermostat_presets(&ain_owned))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))
}

// =============================================================================
/// Dispatches a `comfort`/`eco` valve target or defers to dynamic room-sensor `Offset Control`.
async fn dispatch_preset_valve_target(
    state: &AppState,
    ain: &str,
    dev: Option<&DeviceState>,
    is_comfort: bool,
    target: f64,
) -> ApiResult<()> {
    if should_defer_to_room_sensor_offset(state, dev, target) {
        defer_twin_thermostat_target(&state.registry, ain, target);
        return Ok(());
    }
    let hw_t = compensated_static_hw_target(dev, target);
    let res = if is_comfort {
        state.aha.set_thermostat_comfort(ain, hw_t).await
    } else {
        state.aha.set_thermostat_eco(ain, hw_t).await
    };
    res.map_err(|e| ApiError::logged(e, &state.logger))?;
    sync_twin_thermostat_target(state, ain, target);
    Ok(())
}

// =============================================================================
/// Dispatches `temp` directly (with static offset) or defers to dynamic room-sensor `Offset Control`.
async fn dispatch_or_defer_setpoint(
    state: &AppState,
    ain: &str,
    dev: Option<&DeviceState>,
    temp: f64,
) -> ApiResult<()> {
    if should_defer_to_room_sensor_offset(state, dev, temp) {
        defer_twin_thermostat_target(&state.registry, ain, temp);
        return Ok(());
    }
    let hw_t = compensated_static_hw_target(dev, temp);
    state
        .aha
        .set_target_temperature(ain, hw_t)
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    sync_twin_thermostat_target(state, ain, temp);
    Ok(())
}

// =============================================================================
/// Executes `set_presets` on a thermostat AIN or `"group-<id>"` scope.
pub(super) async fn execute_preset_action(
    state: &AppState,
    target_id: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
) -> ApiResult<()> {
    let comfort_val = parse_optional_temp(params, COMFORT_PARAM_KEYS)?;
    let eco_val = parse_optional_temp(params, ECO_PARAM_KEYS)?;
    if comfort_val.is_none() && eco_val.is_none() {
        return Err(AppError::Validation(
            "Missing 'comfort_temperature' or 'eco_temperature' parameter".into(),
        )
        .into());
    }
    let tid = target_id.to_owned();
    let ((old_c, old_e), (comfort, eco)) = state
        .db
        .run_blocking(move |db| {
            let old = db.get_thermostat_presets(&tid);
            let updated = db.set_thermostat_presets(&tid, comfort_val, eco_val, Some(eff_ts))?;
            Ok((old, updated))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    sync_twin_presets(
        state,
        target_id,
        comfort,
        eco,
        comfort_val.map(|_| old_c),
        eco_val.map(|_| old_e),
    )
    .await;

    if let Some(ref engine) = state.rules_engine {
        engine.evaluate_now().await;
    } else {
        state.registry.set_heating_efficiency(None);
    }
    Ok(())
}

// =============================================================================
/// Executes `set_offset` on a standalone unlinked thermostat with `409 Conflict` checks.
pub(super) async fn execute_offset_action(
    state: &AppState,
    ain: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
) -> ApiResult<()> {
    let raw_off = parse_offset_value(params)?;
    let dev = state.registry.get_device(ain);
    validate_offset_authority(ain, dev.as_ref())?;

    let ain_owned = ain.to_owned();
    let (auth_off, applied) = state
        .db
        .run_blocking(move |db| db.set_thermostat_offset(&ain_owned, &raw_off, Some(eff_ts)))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    if !applied {
        return Ok(());
    }
    apply_static_offset_to_twin_and_valve(state, ain, auth_off).await;
    if let Some(ref engine) = state.rules_engine {
        engine.evaluate_now().await;
    } else {
        state.registry.set_heating_efficiency(None);
    }
    Ok(())
}

// =============================================================================
/// Latches manual user overrides and triggers an immediate `RulesEngine` evaluation.
pub(super) async fn notify_and_evaluate_manual_action(
    state: &AppState,
    ains: &[String],
    action: &str,
) {
    if let Some(ref engine) = state.rules_engine {
        if MANUAL_OVERRIDE_ACTIONS.contains(&action) {
            engine.notify_manual_override(ains).await;
        }
        engine.evaluate_now().await;
    } else {
        state.registry.set_heating_efficiency(None);
    }
}

// =============================================================================
/// Enforces Single-Authority calibration rules (`HTTP 409 Conflict` when locked/linked/Fritz-offset).
fn validate_offset_authority(ain: &str, device: Option<&DeviceState>) -> ApiResult<()> {
    let Some(dev) = device else {
        return Err(AppError::Validation(format!(
            "Device '{ain}' is not a thermostat and does not support 'set_offset'"
        ))
        .into());
    };
    let Some(t) = dev.as_thermostat() else {
        return Err(AppError::Validation(format!(
            "Device '{ain}' is not a thermostat and does not support 'set_offset'"
        ))
        .into());
    };
    if dev.automation_locked || dev.group_id.is_some() {
        let mgr = dev
            .managed_by
            .as_deref()
            .or(dev.group_id.as_deref())
            .unwrap_or("group");
        return Err(AppError::Conflict(format!(
            "Thermostat '{ain}' is managed by group '{mgr}'; static offset is locked"
        ))
        .into());
    }
    if let Some(ref ref_ain) = t.reference_sensor_ain {
        return Err(AppError::Conflict(format!(
            "Thermostat '{ain}' is linked to external room sensor '{ref_ain}'; \
             dynamic Room Sensor Sync has priority over static offset"
        ))
        .into());
    }
    if t.fritz_temperature_offset.abs() >= RULE_TEMP_EPSILON {
        return Err(AppError::Conflict(format!(
            "Thermostat '{ain}' has an active FRITZ!Box hardware offset ({} °C); \
             set FRITZ!Box offset to 0.0 °C first to prevent double compensation",
            t.fritz_temperature_offset
        ))
        .into());
    }
    Ok(())
}

// =============================================================================
/// Returns effective static offset (`°C`) for an unlinked standalone thermostat.
#[must_use]
fn active_static_offset(dev: Option<&DeviceState>) -> f64 {
    let Some(d) = dev else {
        return 0.0;
    };
    let Some(t) = d.as_thermostat() else {
        return 0.0;
    };
    if d.automation_locked
        || t.reference_sensor_ain.is_some()
        || t.fritz_temperature_offset.abs() >= RULE_TEMP_EPSILON
    {
        return 0.0;
    }
    t.temperature_offset
}

// =============================================================================
/// Computes static-offset compensated hardware valve target (`°C`) for `dev`.
#[must_use]
fn compensated_static_hw_target(dev: Option<&DeviceState>, target: f64) -> f64 {
    let eff_off = active_static_offset(dev);
    if eff_off.abs() >= RULE_TEMP_EPSILON {
        calculate_static_offset_target(target, eff_off)
    } else {
        target
    }
}

// =============================================================================
/// Returns `true` when live room-sensor `Offset Control` will dispatch the valve target.
#[must_use]
fn should_defer_to_room_sensor_offset(
    state: &AppState,
    dev: Option<&DeviceState>,
    target: f64,
) -> bool {
    if state.rules_engine.is_none() || state.config_snapshot().automation.vacation_mode {
        return false;
    }
    let Some(d) = dev else {
        return false;
    };
    d.as_thermostat()
        .is_some_and(|t| t.reference_sensor_ain.is_some())
        && can_defer_valve_to_offset_step(d, target, &state.registry)
}

// =============================================================================
/// Updates `temperature_target`, `hardware_target_temperature`, & `target_hold_until` on `ain`.
fn sync_twin_thermostat_target(state: &AppState, ain: &str, user_target: f64) {
    let cur = state.registry.get_device(ain);
    let hw_t = compensated_static_hw_target(cur.as_ref(), user_target);
    let hold = now_epoch_f64() + TARGET_HOLD_SECONDS;
    state.registry.mutate_device(ain, |d| {
        if let Some(t) = d.as_thermostat_mut() {
            t.temperature_target = user_target;
            t.hardware_target_temperature = Some(hw_t);
            t.target_hold_until = hold;
        }
    });
}

// =============================================================================
/// Updates Digital Twin & dispatches compensated valve target after `set_offset`.
async fn apply_static_offset_to_twin_and_valve(state: &AppState, ain: &str, auth_off: f64) {
    let mut valve_cmd: Option<f64> = None;
    state.registry.mutate_device(ain, |d| {
        let rh_opt = d.humidity;
        let Some(t) = d.as_thermostat_mut() else {
            return;
        };
        t.temperature_offset = auth_off;
        if let Some(hw) = t.temperature_hardware.filter(|&v| v > 0.0) {
            t.temperature_current = round_decimals(hw + auth_off, 1);
            if let Some(rh) = rh_opt.filter(|&r| r > 0.0) {
                t.apparent_temperature =
                    Some(calculate_apparent_temperature(t.temperature_current, rh));
            }
        }
        let user_t = t.temperature_target;
        if user_t > MIN_THERMOSTAT_TEMP && user_t <= MAX_THERMOSTAT_TEMP {
            let desired_hw = calculate_static_offset_target(user_t, auth_off);
            let cur_hw = t.hardware_target_temperature.unwrap_or(user_t);
            if (desired_hw - cur_hw).abs() >= (OFFSET_HYSTERESIS_STEP - RULE_TEMP_EPSILON) {
                valve_cmd = Some(desired_hw);
            }
            t.hardware_target_temperature = Some(desired_hw);
            t.target_hold_until = now_epoch_f64() + TARGET_HOLD_SECONDS;
        }
    });
    if let Some(desired_hw) = valve_cmd {
        if let Err(err) = state.aha.set_target_temperature(ain, desired_hw).await {
            state.logger.warning(&format!(
                "Failed dispatching static-offset valve target {desired_hw}°C on {ain}: {err}"
            ));
        }
    }
}

// =============================================================================
/// Synchronizes `(comfort, eco)` presets onto Digital Twin thermostats & updates active preset target.
async fn sync_twin_presets(
    state: &AppState,
    target_id: &str,
    comfort: f64,
    eco: f64,
    old_comfort: Option<f64>,
    old_eco: Option<f64>,
) {
    let ains = resolve_preset_member_ains(state, target_id).await;
    for ain in ains {
        apply_preset_to_member(state, &ain, comfort, eco, old_comfort, old_eco).await;
    }
}

// =============================================================================
/// Resolves all member thermostat AINs controlled by `target_id` (single AIN or group).
async fn resolve_preset_member_ains(state: &AppState, target_id: &str) -> Vec<String> {
    let tid = target_id.to_owned();
    state
        .db
        .run_blocking(move |db| {
            let (st, sid) = db.resolve_preset_scope(&tid)?;
            if st == SCOPE_GROUP {
                let grp = db.get_software_group(&sid)?;
                let members = grp
                    .and_then(|g| g.get("members").and_then(Value::as_array).cloned())
                    .map(|arr| {
                        arr.into_iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(members)
            } else {
                Ok(vec![tid])
            }
        })
        .await
        .unwrap_or_else(|_| vec![target_id.to_owned()])
}

// =============================================================================
/// Updates `(comfort, eco)` on `ain` and nudges the valve if currently tracking `old_comfort`/`old_eco`.
async fn apply_preset_to_member(
    state: &AppState,
    ain: &str,
    comfort: f64,
    eco: f64,
    old_comfort: Option<f64>,
    old_eco: Option<f64>,
) {
    let Some(dev) = state.registry.get_device(ain) else {
        return;
    };
    let Some(cur_t) = dev.as_thermostat().map(|t| t.temperature_target) else {
        return;
    };
    state.registry.mutate_device(ain, |d| {
        if let Some(t) = d.as_thermostat_mut() {
            t.comfort_temperature = Some(comfort);
            t.eco_temperature = Some(eco);
        }
    });
    let Some(new_t) = resolve_active_preset_shift(cur_t, comfort, eco, old_comfort, old_eco) else {
        return;
    };
    if should_defer_to_room_sensor_offset(state, Some(&dev), new_t) {
        defer_twin_thermostat_target(&state.registry, ain, new_t);
        return;
    }
    let hw_t = compensated_static_hw_target(Some(&dev), new_t);
    if let Err(err) = state.aha.set_target_temperature(ain, hw_t).await {
        state.logger.warning(&format!(
            "Failed dispatching updated preset target {hw_t}°C on {ain}: {err}"
        ));
    }
    sync_twin_thermostat_target(state, ain, new_t);
}

// =============================================================================
/// Returns `Some(new_preset)` if `cur_t` matched `old_comfort` or `old_eco` before editing.
#[must_use]
fn resolve_active_preset_shift(
    cur_t: f64,
    comfort: f64,
    eco: f64,
    old_comfort: Option<f64>,
    old_eco: Option<f64>,
) -> Option<f64> {
    if old_comfort.is_some_and(|oc| {
        (cur_t - oc).abs() < RULE_TEMP_EPSILON && (comfort - oc).abs() >= RULE_TEMP_EPSILON
    }) {
        Some(comfort)
    } else if old_eco.is_some_and(|oe| {
        (cur_t - oe).abs() < RULE_TEMP_EPSILON && (eco - oe).abs() >= RULE_TEMP_EPSILON
    }) {
        Some(eco)
    } else {
        None
    }
}
