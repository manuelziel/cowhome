//! Virtual Software Groups CRUD HTTP handlers (`/api/v1/groups` & `/api/v1/groups/{id}`).
//!
//! Enforces Single-Authority exclusive membership (`HTTP 409 Conflict`), LWW `ignored_stale`,
//! and live Digital Twin + `RulesEngine` synchronization after every group mutation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Map, Value, json};

use crate::core::domain_constants::hardware::special_devices::GROUP_ID_PREFIX;
use crate::core::error::AppError;
use crate::core::time_util::normalize_ain;
use crate::models::group::parse_group_id;
use crate::web::response::{
    ApiError, ApiResult, api_created, api_success, extract_request_timestamp, parse_json_object,
};
use crate::web::state::AppState;

// =============================================================================
/// `GET /api/v1/groups` — lists all software groups, sorted members, & group automations.
///
/// # Errors
/// Returns [`ApiError`] if SQLite fails.
pub async fn list_groups(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let groups = state
        .db
        .run_blocking(|db| db.get_all_software_groups())
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    Ok(api_success(json!(groups), "Software groups retrieved"))
}

// =============================================================================
/// `POST /api/v1/groups` — creates a new virtual software group (`201 Created`).
///
/// # Errors
/// Returns [`ApiError`] (`400` / `409` / `503`) on invalid payload or duplicate membership.
pub async fn create_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let payload = parse_json_object(&body)?;
    let (eff_ts, is_explicit) = extract_request_timestamp(Some(&payload), None, &headers)?;
    let (name, members) = validate_create_payload(&payload)?;
    let ts_opt = is_explicit.then_some(eff_ts);

    let created = state
        .db
        .run_blocking(move |db| db.create_software_group(&name, &members, ts_opt))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    sync_registry_group_authority(&state).await;
    let gid = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let refreshed = state
        .db
        .run_blocking(move |db| db.get_software_group(&gid))
        .await
        .ok()
        .flatten()
        .unwrap_or(created);

    Ok(api_created(refreshed, "Software group created"))
}

// =============================================================================
/// `GET /api/v1/groups/{id}` — fetches details and automations of a single software group.
///
/// # Errors
/// Returns [`ApiError`] (`404 Not Found`) if the software group does not exist.
pub async fn get_group_item(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
) -> ApiResult<Response> {
    let gid = group_id.clone();
    let group = state
        .db
        .run_blocking(move |db| db.get_software_group(&gid))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?
        .ok_or_else(|| AppError::NotFound(format!("Software group '{group_id}' not found")))?;
    Ok(api_success(group, "Software group retrieved"))
}

// =============================================================================
/// `PUT /api/v1/groups/{id}` — updates `name` and/or `members` with LWW protection.
///
/// # Errors
/// Returns [`ApiError`] (`400` / `404` / `409`) on bad payload, missing group, or conflict.
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let payload = parse_json_object(&body)?;
    let (eff_ts, _) = extract_request_timestamp(Some(&payload), None, &headers)?;
    let (name_opt, members_opt) = validate_update_payload(&payload)?;
    let gid = group_id.clone();

    let updated = state
        .db
        .run_blocking(move |db| {
            db.update_software_group(
                &gid,
                name_opt.as_deref(),
                members_opt.as_deref(),
                Some(eff_ts),
            )
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?
        .ok_or_else(|| AppError::NotFound(format!("Software group '{group_id}' not found")))?;

    let ignored_stale = updated
        .get("ignored_stale")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ignored_stale {
        sync_registry_group_authority(&state).await;
    }
    let msg = if ignored_stale {
        "Stale software group update ignored"
    } else {
        "Software group updated"
    };
    Ok(api_success(updated, msg))
}

// =============================================================================
/// `DELETE /api/v1/groups/{id}` — deletes a software group and unlocks its member devices.
///
/// # Errors
/// Returns [`ApiError`] (`404 Not Found`) if the software group does not exist.
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
) -> ApiResult<Response> {
    let gid = group_id.clone();
    let deleted = state
        .db
        .run_blocking(move |db| db.delete_software_group(&gid))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    if !deleted {
        return Err(AppError::NotFound(format!("Software group '{group_id}' not found")).into());
    }
    let canon_id = parse_group_id(&group_id).map_or(group_id, |id| id.to_string());
    sync_registry_group_authority(&state).await;
    Ok(api_success(
        json!({ "status": "success", "id": canon_id }),
        "Software group deleted",
    ))
}

// =============================================================================
/// Synchronizes software-group Single-Authority fields onto `StateRegistry` and re-runs `RulesEngine`.
pub async fn sync_registry_group_authority(state: &AppState) {
    let all_ains: Vec<String> = state
        .registry
        .get_all_devices()
        .into_iter()
        .map(|d| d.ain)
        .collect();
    let (sw_map, presets_map) = state
        .db
        .run_blocking(move |db| {
            let mut m_map: HashMap<String, (String, String)> = HashMap::new();
            for g in db.get_all_software_groups().unwrap_or_default() {
                let gid = g.get("id").and_then(Value::as_str).unwrap_or("");
                let gname = g.get("name").and_then(Value::as_str).unwrap_or("");
                let pref = format!("{GROUP_ID_PREFIX}{gid}");
                if let Some(arr) = g.get("members").and_then(Value::as_array) {
                    for mem in arr.iter().filter_map(Value::as_str) {
                        m_map.insert(normalize_ain(mem), (pref.clone(), gname.to_owned()));
                    }
                }
            }
            let p_map: HashMap<String, (f64, f64)> = all_ains
                .into_iter()
                .map(|a| {
                    let p = db.get_thermostat_presets(&a);
                    (a, p)
                })
                .collect();
            Ok((m_map, p_map))
        })
        .await
        .unwrap_or_default();

    for dev in state.registry.get_all_devices() {
        let found = sw_map.get(&normalize_ain(&dev.ain)).cloned();
        let presets = presets_map.get(&dev.ain).copied();
        state.registry.mutate_device(&dev.ain, |d| {
            match found {
                Some((gid, gname)) => {
                    d.group_id = Some(gid.clone());
                    d.group_ain = Some(gid);
                    d.automation_locked = true;
                    d.managed_by = Some(gname);
                }
                None => {
                    d.group_id = None;
                    d.group_ain = None;
                    d.automation_locked = false;
                    d.managed_by = None;
                }
            }
            if let (Some(t), Some((c, e))) = (d.as_thermostat_mut(), presets) {
                t.comfort_temperature = Some(c);
                t.eco_temperature = Some(e);
            }
        });
    }

    if let Some(ref engine) = state.rules_engine {
        engine.invalidate_schedule_cache().await;
        engine.evaluate_now().await;
    } else {
        state.registry.set_heating_efficiency(None);
    }
}

// =============================================================================
/// Validates `POST /api/v1/groups` body (`name` + optional `members` array).
fn validate_create_payload(payload: &Map<String, Value>) -> ApiResult<(String, Vec<String>)> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation("Group 'name' is required and must be a non-empty string".into())
        })?
        .to_owned();
    let members = match payload.get("members") {
        None => Vec::new(),
        Some(val) => parse_members_array(val, "Group 'members' must be a list of device AINs")?,
    };
    Ok((name, members))
}

// =============================================================================
/// Validates `PUT /api/v1/groups/{id}` body (`name` and/or `members`).
fn validate_update_payload(
    payload: &Map<String, Value>,
) -> ApiResult<(Option<String>, Option<Vec<String>>)> {
    if !payload.contains_key("name") && !payload.contains_key("members") {
        return Err(
            AppError::Validation("Payload must contain 'name' and/or 'members'".into()).into(),
        );
    }
    let name_opt = match payload.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let s = v
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::Validation("'name' must be a non-empty string".into()))?;
            Some(s.to_owned())
        }
    };
    let members_opt = match payload.get("members") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_members_array(
            v,
            "'members' must be a list of device AINs",
        )?),
    };
    Ok((name_opt, members_opt))
}

// =============================================================================
/// Parses and deduplicates a JSON array of member AIN strings/values.
fn parse_members_array(val: &Value, err_msg: &str) -> ApiResult<Vec<String>> {
    let arr = val
        .as_array()
        .ok_or_else(|| AppError::Validation(err_msg.into()))?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let raw = match item {
            Value::String(s) => s.trim().to_owned(),
            other if !other.is_null() => other.to_string().trim().to_owned(),
            _ => String::new(),
        };
        if !raw.is_empty() && seen.insert(raw.clone()) {
            out.push(raw);
        }
    }
    Ok(out)
}
