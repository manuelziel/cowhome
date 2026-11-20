//! `POST /api/v1/devices/{id}/action` actuator router for AHA devices, groups, & `ROUTER_01`.
//!
//! Enforces Last-Write-Wins (`ignored_stale`), `300 s` Command TTL on physical relays/reboots,
//! and automatic software-group escalation, delegating thermostat calibration & presets to
//! [`super::device_thermostat`].

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{json, Map, Value};

use super::device_helpers::{
    build_action_lww_keys, filter_group_action_members, is_action_stale, parse_bool_param,
    parse_int_param, parse_name_param, validate_aha_action, validate_router_action,
};
use super::device_thermostat::{
    execute_offset_action, execute_preset_action, execute_thermostat_action,
    notify_and_evaluate_manual_action,
};
use crate::core::domain_constants::hardware::{
    device_actions, device_capabilities,
    fritz_api::{
        MAX_COLOR_HUE, MAX_COLOR_SATURATION, MAX_COLOR_TEMP_KELVIN, MAX_LEVEL_PERCENT,
        MIN_COLOR_HUE, MIN_COLOR_SATURATION, MIN_COLOR_TEMP_KELVIN, MIN_LEVEL_PERCENT,
    },
    router_actions,
    special_devices::{GROUP_ID_PREFIX, ROUTER_AIN},
};
use crate::core::error::AppError;
use crate::web::response::{
    api_success, extract_request_timestamp, parse_json_object, ApiError, ApiResult,
};
use crate::web::state::AppState;

/// Actions automatically escalated to the controlling software group when sent to a member AIN.
const GROUP_ESCALATED_ACTIONS: &[&str] = &[
    device_actions::SET_TEMPERATURE,
    device_actions::SET_COMFORT,
    device_actions::SET_ECO,
    device_actions::SET_PRESETS,
];

// =============================================================================
/// `POST /api/v1/devices/{id}/action` — dispatches router, group, or Smart Home device commands.
///
/// # Errors
/// Returns [`ApiError`] (`400` / `404` / `409` / `503`) on validation, lookup, or gateway error.
pub async fn post_device_action(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let payload = parse_json_object(&body)?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Validation("Missing 'action' field in request body".into()))?
        .to_owned();
    let params = payload
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let (eff_ts, is_explicit) = extract_request_timestamp(Some(&payload), Some(&params), &headers)?;

    if device_id == ROUTER_AIN {
        return execute_router_post(&state, &device_id, &action, &params, eff_ts, is_explicit)
            .await;
    }

    let Some(dev) = state.registry.get_device(&device_id) else {
        if let Some(resp) =
            try_software_group_action(&state, &device_id, &action, &params, eff_ts, is_explicit)
                .await?
        {
            return Ok(resp);
        }
        return Err(AppError::NotFound(format!(
            "Device with AIN {device_id} not found in Digital Twin"
        ))
        .into());
    };

    if let Some(ref gid) = dev.group_id {
        if GROUP_ESCALATED_ACTIONS.contains(&action.as_str()) {
            if let Some(resp) =
                try_software_group_action(&state, gid, &action, &params, eff_ts, is_explicit)
                    .await?
            {
                return Ok(resp);
            }
        }
    }

    execute_single_device_post(&state, &dev.ain, &action, &params, eff_ts, is_explicit).await
}

// =============================================================================
/// Validates, LWW/TTL-checks, and executes a `ROUTER_01` TR-064 action.
async fn execute_router_post(
    state: &AppState,
    device_id: &str,
    action: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
    is_explicit: bool,
) -> ApiResult<Response> {
    validate_router_action(action, params)?;
    let keys = build_action_lww_keys(state, device_id, action, &[]).await;
    if is_action_stale(state, &keys, action, eff_ts, is_explicit) {
        return Ok(api_success(
            json!({ "action": action, "device": device_id, "ignored_stale": true }),
            "Stale router action ignored",
        ));
    }

    match action {
        router_actions::RECONNECT => state
            .tr064
            .reconnect_internet()
            .await
            .map_err(|e| ApiError::logged(e, &state.logger))?,
        router_actions::REBOOT => state
            .tr064
            .reboot_router()
            .await
            .map_err(|e| ApiError::logged(e, &state.logger))?,
        router_actions::SET_WLAN => {
            let en = parse_bool_param(params, "enabled")?;
            state
                .tr064
                .set_wlan(en)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))?;
        }
        router_actions::SET_GUEST_WLAN => {
            let en = parse_bool_param(params, "enabled")?;
            state
                .tr064
                .set_guest_wlan(en)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))?;
        }
        router_actions::WAKE_ON_LAN => {
            let mac = params
                .get("mac_address")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            state
                .tr064
                .wake_on_lan(mac)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))?;
        }
        router_actions::SET_NAME => {
            let clean = parse_name_param(params)?;
            let name_clone = clean.clone();
            let applied = state
                .db
                .run_blocking(move |db| db.set_custom_name(ROUTER_AIN, &name_clone, Some(eff_ts)))
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))?;
            if applied {
                state.registry.mutate_device(ROUTER_AIN, |d| d.name = clean);
            }
        }
        _ => {}
    }

    state.db.record_lww_mutation(&keys, eff_ts);
    Ok(api_success(
        json!({ "action": action, "device": device_id, "ignored_stale": false }),
        "Router action executed",
    ))
}

// =============================================================================
/// Validates, LWW/TTL-checks, and executes an action on a single AHA Smart Home device.
async fn execute_single_device_post(
    state: &AppState,
    ain: &str,
    action: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
    is_explicit: bool,
) -> ApiResult<Response> {
    validate_aha_action(action, params)?;
    let keys = build_action_lww_keys(state, ain, action, &[]).await;
    if is_action_stale(state, &keys, action, eff_ts, is_explicit) {
        return Ok(api_success(
            json!({ "action": action, "device": ain, "ignored_stale": true }),
            "Stale Smart Home action ignored",
        ));
    }
    dispatch_aha_action(state, ain, action, params, eff_ts, true).await?;
    state.db.record_lww_mutation(&keys, eff_ts);
    Ok(api_success(
        json!({ "action": action, "device": ain, "ignored_stale": false }),
        "Smart Home action executed",
    ))
}

// =============================================================================
/// Attempts to resolve `group_id` as a software group and fan out `action` across members.
async fn try_software_group_action(
    state: &AppState,
    group_id: &str,
    action: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
    is_explicit: bool,
) -> ApiResult<Option<Response>> {
    let raw_id = group_id.trim_start_matches(GROUP_ID_PREFIX).to_owned();
    let group_opt = state
        .db
        .run_blocking(move |db| db.get_software_group(&raw_id))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    let Some(group) = group_opt else {
        return Ok(None);
    };
    if action == device_actions::SET_OFFSET {
        return Err(AppError::Conflict(
            "Static temperature_offset cannot be set on a software group; \
             group climate is managed by Room Sensor Sync (ApparentTemperature)"
                .into(),
        )
        .into());
    }
    validate_aha_action(action, params)?;

    let gid_str = group.get("id").and_then(Value::as_str).unwrap_or("");
    let raw_members: Vec<String> = group
        .get("members")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let targets = filter_group_action_members(state, &raw_members, action);
    let prefixed_gid = format!("{GROUP_ID_PREFIX}{gid_str}");
    let keys = build_action_lww_keys(state, &prefixed_gid, action, &targets).await;

    if eff_ts > 0.0 && is_action_stale(state, &keys, action, eff_ts, is_explicit) {
        return Ok(Some(api_success(
            json!({
                "action": action,
                "group_id": gid_str,
                "members_count": targets.len(),
                "ignored_stale": true,
            }),
            "Stale group action ignored",
        )));
    }

    if action == device_actions::SET_PRESETS {
        execute_preset_action(state, &prefixed_gid, params, eff_ts).await?;
    } else {
        for member_ain in &targets {
            if let Err(err) =
                dispatch_aha_action(state, member_ain, action, params, eff_ts, false).await
            {
                if matches!(err.err, AppError::Validation(_)) {
                    return Err(err);
                }
                state.logger.warning(&format!(
                    "Action '{action}' failed on group member {member_ain}: {}",
                    err.err
                ));
            }
        }
        notify_and_evaluate_manual_action(state, &targets, action).await;
    }

    if eff_ts > 0.0 {
        state.db.record_lww_mutation(&keys, eff_ts);
    }
    Ok(Some(api_success(
        json!({
            "action": action,
            "group_id": gid_str,
            "members_count": targets.len(),
            "ignored_stale": false,
        }),
        &format!("Group action executed on {} devices", targets.len()),
    )))
}

// =============================================================================
/// Routes a validated Smart Home `action` to `FritzAhaClient` and updates `StateRegistry`.
async fn dispatch_aha_action(
    state: &AppState,
    ain: &str,
    action: &str,
    params: &Map<String, Value>,
    eff_ts: f64,
    trigger_engine: bool,
) -> ApiResult<()> {
    match action {
        device_actions::SET_SWITCH | device_actions::TOGGLE_SWITCH => {
            execute_switch_action(state, ain, action, params, trigger_engine).await
        }
        device_actions::SET_TEMPERATURE | device_actions::SET_COMFORT | device_actions::SET_ECO => {
            execute_thermostat_action(state, ain, action, params, eff_ts, trigger_engine).await
        }
        device_actions::SET_PRESETS => execute_preset_action(state, ain, params, eff_ts).await,
        device_actions::SET_BLIND => {
            let st = params.get("state").and_then(Value::as_str).unwrap_or("");
            state
                .aha
                .set_blind_state(ain, st)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))
        }
        device_actions::SET_LEVEL => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let lvl = parse_int_param(params, "level")?
                .clamp(i64::from(MIN_LEVEL_PERCENT), i64::from(MAX_LEVEL_PERCENT))
                as u8;
            state
                .aha
                .set_level(ain, lvl)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))
        }
        device_actions::SET_COLOR => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let hue = parse_int_param(params, "hue")?
                .clamp(i64::from(MIN_COLOR_HUE), i64::from(MAX_COLOR_HUE))
                as u16;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let sat = parse_int_param(params, "saturation")?.clamp(
                i64::from(MIN_COLOR_SATURATION),
                i64::from(MAX_COLOR_SATURATION),
            ) as u8;
            state
                .aha
                .set_color(ain, hue, sat)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))
        }
        device_actions::SET_COLOR_TEMP | device_actions::SET_COLOR_TEMP_ALIAS => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let kelvin = parse_int_param(params, "kelvin")?.clamp(
                i64::from(MIN_COLOR_TEMP_KELVIN),
                i64::from(MAX_COLOR_TEMP_KELVIN),
            ) as u16;
            state
                .aha
                .set_color_temperature(ain, kelvin)
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))
        }
        device_actions::SET_NAME => {
            let clean = parse_name_param(params)?;
            let ain_owned = ain.to_owned();
            let name_clone = clean.clone();
            let applied = state
                .db
                .run_blocking(move |db| db.set_custom_name(&ain_owned, &name_clone, Some(eff_ts)))
                .await
                .map_err(|e| ApiError::logged(e, &state.logger))?;
            if applied {
                state.registry.mutate_device(ain, |d| d.name = clean);
            }
            Ok(())
        }
        device_actions::SET_OFFSET => execute_offset_action(state, ain, params, eff_ts).await,
        _ => Err(AppError::Validation(format!("Unknown device action: {action}")).into()),
    }
}

// =============================================================================
/// Executes `set_switch` or `toggle_switch`, rejecting read-only power meters (`HTTP 400`).
async fn execute_switch_action(
    state: &AppState,
    ain: &str,
    action: &str,
    params: &Map<String, Value>,
    trigger_engine: bool,
) -> ApiResult<()> {
    let dev = state.registry.get_device(ain);
    if let Some(ref d) = dev {
        if !d.capabilities.is_empty()
            && !d
                .capabilities
                .iter()
                .any(|c| c == device_capabilities::SWITCH)
        {
            return Err(AppError::Validation(format!(
                "Device '{ain}' is a read-only power meter and cannot be switched"
            ))
            .into());
        }
    }
    if action == device_actions::SET_SWITCH {
        let on = parse_bool_param(params, "state")?;
        state
            .aha
            .set_switch(ain, on)
            .await
            .map_err(|e| ApiError::logged(e, &state.logger))?;
        state.registry.mutate_device(ain, |d| {
            if let Some(sw) = d.as_switch_mut() {
                sw.state = on;
            }
        });
    } else {
        state
            .aha
            .toggle_switch(ain)
            .await
            .map_err(|e| ApiError::logged(e, &state.logger))?;
        state.registry.mutate_device(ain, |d| {
            if let Some(sw) = d.as_switch_mut() {
                sw.state = !sw.state;
            }
        });
    }
    if trigger_engine {
        notify_and_evaluate_manual_action(state, &[ain.to_owned()], action).await;
    }
    Ok(())
}
