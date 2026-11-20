//! Parameter parsers, upfront validation, & LWW/TTL helpers for `/devices/{id}/action`.
//!
//! Separated from [`super::device_action`] and [`super::device_thermostat`] so parameter
//! extraction and Last-Write-Wins / Command-TTL checks stay concise (`<= 380` lines).

use serde_json::{Map, Value};

use crate::core::domain_constants::{
    automation::automation_identifiers::{
        COMFORT_ACTION_KEYS, COMFORT_PARAM_KEYS, ECO_ACTION_KEYS, ECO_PARAM_KEYS, HEATING_SCHEDULE,
    },
    hardware::{
        blind_states, device_actions, device_capabilities, device_types,
        fritz_api::{
            HALF_DEGREE_FACTOR, MAX_COLOR_HUE, MAX_COLOR_SATURATION, MAX_COLOR_TEMP_KELVIN,
            MAX_LEVEL_PERCENT, MAX_THERMOSTAT_OFFSET, MAX_THERMOSTAT_TEMP, MIN_COLOR_HUE,
            MIN_COLOR_SATURATION, MIN_COLOR_TEMP_KELVIN, MIN_LEVEL_PERCENT, MIN_THERMOSTAT_OFFSET,
            MIN_THERMOSTAT_TEMP, OFFSET_SETTING_KEY_PREFIX,
        },
        router_actions,
        special_devices::ROUTER_AIN,
    },
};
use crate::core::error::AppError;
use crate::core::time_util::{
    build_action_lww_key, build_automation_lww_key, build_name_lww_key, build_setting_lww_key,
    is_command_expired, normalize_ain,
};
use crate::web::response::ApiResult;
use crate::web::state::AppState;

/// Candidate parameter keys carrying static calibration offset (`set_offset`).
const OFFSET_PARAM_KEYS: &[&str] = &["offset", "temperature_offset"];

// =============================================================================
/// Builds LWW resource tracking keys affected by `(target_id, action, member_ains)`.
pub(super) async fn build_action_lww_keys(
    state: &AppState,
    target_id: &str,
    action: &str,
    member_ains: &[String],
) -> Vec<String> {
    let norm = normalize_ain(target_id);
    if action == device_actions::SET_NAME {
        return vec![build_name_lww_key(target_id)];
    }
    if action == device_actions::SET_OFFSET {
        return vec![build_setting_lww_key(&format!(
            "{OFFSET_SETTING_KEY_PREFIX}{norm}"
        ))];
    }
    if target_id == ROUTER_AIN {
        return vec![format!("action:{norm}:{action}")];
    }
    let mut keys = vec![build_action_lww_key(target_id)];
    keys.extend(member_ains.iter().map(|m| build_action_lww_key(m)));
    if action == device_actions::SET_PRESETS {
        let tid = target_id.to_owned();
        if let Ok((st, sid)) = state
            .db
            .run_blocking(move |db| db.resolve_preset_scope(&tid))
            .await
        {
            keys.push(build_automation_lww_key(&st, &sid, HEATING_SCHEDULE));
        }
    }
    keys
}

// =============================================================================
/// Returns `true` when `(action, eff_ts, is_explicit)` is expired by TTL or stale by LWW.
#[must_use]
pub(super) fn is_action_stale(
    state: &AppState,
    keys: &[String],
    action: &str,
    eff_ts: f64,
    is_explicit: bool,
) -> bool {
    is_command_expired(action, eff_ts, is_explicit, None) || state.db.is_lww_stale(keys, eff_ts)
}

// =============================================================================
/// Filters software-group `members` down to actuators compatible with `action`.
#[must_use]
pub(super) fn filter_group_action_members(
    state: &AppState,
    members: &[String],
    action: &str,
) -> Vec<String> {
    let is_sw = matches!(
        action,
        device_actions::SET_SWITCH | device_actions::TOGGLE_SWITCH
    );
    let is_th = matches!(
        action,
        device_actions::SET_TEMPERATURE | device_actions::SET_COMFORT | device_actions::SET_ECO
    );
    if !is_sw && !is_th {
        return members.to_vec();
    }
    members
        .iter()
        .filter(|m| {
            let Some(dev) = state.registry.get_device(m) else {
                return true;
            };
            if is_sw {
                dev.device_type == device_types::SWITCH
                    && (dev.capabilities.is_empty()
                        || dev
                            .capabilities
                            .iter()
                            .any(|c| c == device_capabilities::SWITCH))
            } else {
                matches!(
                    dev.device_type.as_str(),
                    device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
                )
            }
        })
        .cloned()
        .collect()
}

// =============================================================================
/// Validates a `ROUTER_01` action and its required `params` upfront.
pub(super) fn validate_router_action(action: &str, params: &Map<String, Value>) -> ApiResult<()> {
    match action {
        router_actions::RECONNECT | router_actions::REBOOT => Ok(()),
        router_actions::SET_WLAN | router_actions::SET_GUEST_WLAN => {
            parse_bool_param(params, "enabled").map(|_| ())
        }
        router_actions::WAKE_ON_LAN => {
            let ok = params
                .get("mac_address")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
            if !ok {
                return Err(AppError::Validation("Missing 'mac_address' parameter".into()).into());
            }
            Ok(())
        }
        router_actions::SET_NAME => parse_name_param(params).map(|_| ()),
        _ => Err(AppError::Validation(format!("Unknown router action: {action}")).into()),
    }
}

// =============================================================================
/// Validates a Smart Home AHA `action` and its required `params` upfront.
pub(super) fn validate_aha_action(action: &str, params: &Map<String, Value>) -> ApiResult<()> {
    match action {
        device_actions::SET_SWITCH => parse_bool_param(params, "state").map(|_| ()),
        device_actions::TOGGLE_SWITCH => Ok(()),
        device_actions::SET_TEMPERATURE => {
            let temp = parse_float_param(params, "temperature")?;
            if temp < 0.0 || (temp > 0.0 && temp < MIN_THERMOSTAT_TEMP) {
                return Err(AppError::Validation(format!(
                    "'temperature' must be 0.0 (OFF) or >= {MIN_THERMOSTAT_TEMP} °C"
                ))
                .into());
            }
            Ok(())
        }
        device_actions::SET_PRESETS => {
            let c = parse_optional_temp(params, COMFORT_PARAM_KEYS)?;
            let e = parse_optional_temp(params, ECO_PARAM_KEYS)?;
            if c.is_none() && e.is_none() {
                return Err(AppError::Validation(
                    "Missing 'comfort_temperature' or 'eco_temperature' parameter".into(),
                )
                .into());
            }
            Ok(())
        }
        device_actions::SET_COMFORT => parse_optional_temp(params, COMFORT_ACTION_KEYS).map(|_| ()),
        device_actions::SET_ECO => parse_optional_temp(params, ECO_ACTION_KEYS).map(|_| ()),
        device_actions::SET_BLIND => {
            let st = params.get("state").and_then(Value::as_str).unwrap_or("");
            if !blind_states::ALL.contains(&st) {
                return Err(AppError::Validation(format!(
                    "Invalid blind state (must be {})",
                    blind_states::ALL.join(", ")
                ))
                .into());
            }
            Ok(())
        }
        device_actions::SET_LEVEL => parse_bounded_int(
            params,
            "level",
            i64::from(MIN_LEVEL_PERCENT),
            i64::from(MAX_LEVEL_PERCENT),
        )
        .map(|_| ()),
        device_actions::SET_COLOR => {
            parse_bounded_int(
                params,
                "hue",
                i64::from(MIN_COLOR_HUE),
                i64::from(MAX_COLOR_HUE),
            )?;
            parse_bounded_int(
                params,
                "saturation",
                i64::from(MIN_COLOR_SATURATION),
                i64::from(MAX_COLOR_SATURATION),
            )
            .map(|_| ())
        }
        device_actions::SET_COLOR_TEMP | device_actions::SET_COLOR_TEMP_ALIAS => parse_bounded_int(
            params,
            "kelvin",
            i64::from(MIN_COLOR_TEMP_KELVIN),
            i64::from(MAX_COLOR_TEMP_KELVIN),
        )
        .map(|_| ()),
        device_actions::SET_NAME => parse_name_param(params).map(|_| ()),
        device_actions::SET_OFFSET => parse_offset_value(params).map(|_| ()),
        _ => Err(AppError::Validation(format!("Unknown device action: {action}")).into()),
    }
}

// =============================================================================
/// Parses a required boolean parameter (`true`/`false`, `1`/`0`, `"true"`/`"false"`).
pub(super) fn parse_bool_param(params: &Map<String, Value>, key: &str) -> ApiResult<bool> {
    let val = params
        .get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::Validation(format!("Missing '{key}' parameter")))?;
    match val {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => {
            Ok(n.as_i64() == Some(1))
        }
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(AppError::Validation(format!("Invalid boolean value for '{key}'")).into()),
        },
        _ => Err(AppError::Validation(format!("Invalid boolean value for '{key}'")).into()),
    }
}

// =============================================================================
/// Parses a required floating-point parameter, rejecting booleans and `null`.
pub(super) fn parse_float_param(params: &Map<String, Value>, key: &str) -> ApiResult<f64> {
    let val = params
        .get(key)
        .filter(|v| !v.is_null() && !v.is_boolean())
        .ok_or_else(|| AppError::Validation(format!("Missing or invalid '{key}' parameter")))?;
    let num = match val {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|v| v.is_finite())
    .ok_or_else(|| AppError::Validation(format!("Invalid numeric value for '{key}'")))?;
    Ok(num)
}

// =============================================================================
/// Parses a required integer parameter, rejecting booleans and `null`.
pub(super) fn parse_int_param(params: &Map<String, Value>, key: &str) -> ApiResult<i64> {
    let val = params
        .get(key)
        .filter(|v| !v.is_null() && !v.is_boolean())
        .ok_or_else(|| AppError::Validation(format!("Missing or invalid '{key}' parameter")))?;
    let num = match val {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
    .ok_or_else(|| AppError::Validation(format!("Invalid integer value for '{key}'")))?;
    Ok(num)
}

// =============================================================================
/// Parses a required integer parameter and enforces `[min_val, max_val]` per `openapi.yaml`.
fn parse_bounded_int(
    params: &Map<String, Value>,
    key: &str,
    min_val: i64,
    max_val: i64,
) -> ApiResult<i64> {
    let val = parse_int_param(params, key)?;
    if !(min_val..=max_val).contains(&val) {
        return Err(AppError::Validation(format!(
            "'{key}' must be between {min_val} and {max_val}"
        ))
        .into());
    }
    Ok(val)
}

// =============================================================================
/// Parses a required non-empty `"name"` string parameter.
pub(super) fn parse_name_param(params: &Map<String, Value>) -> ApiResult<String> {
    params
        .get("name")
        .and_then(|v| v.as_str().map(str::trim))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AppError::Validation("Missing or empty 'name' parameter".into()).into())
}

// =============================================================================
/// Parses an optional thermostat preset temperature in `[8.0, 28.0] °C`.
pub(super) fn parse_optional_temp(
    params: &Map<String, Value>,
    keys: &[&str],
) -> ApiResult<Option<f64>> {
    for &key in keys {
        if params.get(key).is_some_and(|v| !v.is_null()) {
            let val = parse_float_param(params, key)?;
            if !(MIN_THERMOSTAT_TEMP..=MAX_THERMOSTAT_TEMP).contains(&val) {
                return Err(AppError::Validation(format!(
                    "'{key}' must be between {MIN_THERMOSTAT_TEMP} and {MAX_THERMOSTAT_TEMP} °C"
                ))
                .into());
            }
            return Ok(Some(val));
        }
    }
    Ok(None)
}

// =============================================================================
/// Extracts and validates the raw JSON `"offset"` (`-5.0..=+5.0 °C` in `0.5 °C` steps).
pub(super) fn parse_offset_value(params: &Map<String, Value>) -> ApiResult<Value> {
    for &key in OFFSET_PARAM_KEYS {
        if let Some(raw) = params.get(key).filter(|v| !v.is_null()) {
            let val = parse_float_param(params, key)?;
            if !(MIN_THERMOSTAT_OFFSET..=MAX_THERMOSTAT_OFFSET).contains(&val) {
                return Err(AppError::Validation(format!(
                    "offset must be between {MIN_THERMOSTAT_OFFSET} and {MAX_THERMOSTAT_OFFSET} °C"
                ))
                .into());
            }
            let rounded = (val * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR;
            if (val - rounded).abs() > 1e-4 {
                return Err(
                    AppError::Validation("offset must be in 0.5 °C increments".into()).into(),
                );
            }
            return Ok(raw.clone());
        }
    }
    Err(AppError::Validation("Missing 'offset' parameter".into()).into())
}
