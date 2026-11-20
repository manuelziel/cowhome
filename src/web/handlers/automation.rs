//! Scoped Smart Automation HTTP handlers (`/api/v1/automation` & `/api/v1/automation/{feature}`).
//!
//! Supports `global`, `group`, and `device` scopes, resolves `PowerSchedule` aliases,
//! enforces Single-Authority group locks (`HTTP 409 Conflict`), applies LWW protection,
//! and triggers immediate `RulesEngine` evaluation.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::core::domain_constants::automation::automation_identifiers::{
    ADAPTIVE_START, HEATING_SCHEDULE, KEY_COMFORT_TEMP, KEY_ECO_TEMP, SCOPE_GLOBAL,
};
use crate::core::error::AppError;
use crate::models::automation::{normalize_feature_name, AutomationScope};
use crate::web::response::{
    api_success, extract_request_timestamp, parse_json_object, ApiError, ApiResult,
};
use crate::web::state::AppState;

/// Metadata keys excluded when extracting feature maps from batch `PUT /api/v1/automation`.
const BATCH_META_KEYS: &[&str] = &[
    "scope",
    "scope_type",
    "scope_id",
    "timestamp",
    "client_timestamp",
];

/// Accepted boolean activation field aliases in feature payloads.
const ACTIVE_KEYS: &[&str] = &["is_active", "enabled", "active"];

/// Top-level `HeatingSchedule` shorthand keys accepted without an outer `"config"` wrapper.
const SCHEDULE_SHORTHAND_KEYS: &[&str] = &[
    "days",
    "schedule",
    "preheat_enabled",
    "early_start_enabled",
    "early_start",
    "adaptive_start_enabled",
    KEY_COMFORT_TEMP,
    KEY_ECO_TEMP,
];

// =============================================================================
/// `GET /api/v1/automation` — fetches all 7 automation rules for `(scope, scope_id)`.
///
/// # Errors
/// Returns [`ApiError`] (`400` / `409` / `503`) on invalid scope or grouped device lock.
pub async fn get_automation_all(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let (scope_type, scope_id) = extract_scope(&query, None);
    let data = state
        .db
        .run_blocking(move |db| db.get_all_automation_configs(&scope_type, &scope_id))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    Ok(api_success(data, "Automation configurations retrieved"))
}

// =============================================================================
/// `PUT /api/v1/automation` — updates multiple automation features atomically with LWW.
///
/// # Errors
/// Returns [`ApiError`] (`400` / `409` / `503`) on invalid features, scope, or group lock.
pub async fn put_automation_batch(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let payload = parse_json_object(&body)?;
    if payload.is_empty() {
        return Err(AppError::Validation(
            "Payload must be a non-empty dictionary of feature configurations".into(),
        )
        .into());
    }
    let (eff_ts, _) = extract_request_timestamp(Some(&payload), None, &headers)?;
    let (raw_stype, raw_sid) = extract_scope(&query, Some(&payload));
    let scope = AutomationScope::parse(Some(&raw_stype), Some(&raw_sid))?;
    let features = extract_batch_features(&payload)?;
    let has_sched =
        features.contains_key(HEATING_SCHEDULE) || features.contains_key(ADAPTIVE_START);

    let st_clone = scope.scope_type.clone();
    let sid_clone = scope.scope_id.clone();
    let applied = state
        .db
        .run_blocking(move |db| {
            db.set_automation_configs_batch(&features, &st_clone, &sid_clone, Some(eff_ts))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    if applied {
        trigger_rules_evaluation(&state, &scope.scope_type, &scope.scope_id, has_sched).await;
    }
    let msg = if applied {
        "Automation configurations updated"
    } else {
        "Stale automation update ignored"
    };
    Ok(api_success(
        json!({
            "status": "success",
            "scope": scope.scope_type,
            "scope_type": scope.scope_type,
            "scope_id": scope.scope_id,
            "ignored_stale": !applied,
        }),
        msg,
    ))
}

// =============================================================================
/// `GET /api/v1/automation/{feature}` — fetches one automation feature in `(scope, scope_id)`.
///
/// # Errors
/// Returns [`ApiError`] (`404` / `400` / `409`) if `feature` is unknown or device is locked.
pub async fn get_automation_feature(
    State(state): State<Arc<AppState>>,
    Path(feature_raw): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let canonical = normalize_feature_name(&feature_raw).ok_or_else(|| {
        AppError::NotFound(format!("Automation feature '{feature_raw}' not found"))
    })?;
    let (scope_type, scope_id) = extract_scope(&query, None);
    let data = state
        .db
        .run_blocking(move |db| db.get_automation_config(canonical, &scope_type, &scope_id))
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?
        .ok_or_else(|| {
            AppError::NotFound(format!("Automation feature '{feature_raw}' not found"))
        })?;
    Ok(api_success(data, "Automation feature retrieved"))
}

// =============================================================================
/// `PUT /api/v1/automation/{feature}` — updates `is_active` and/or `config` of one feature.
///
/// # Errors
/// Returns [`ApiError`] (`404` / `400` / `409`) on unknown feature, bad body, or group lock.
pub async fn put_automation_feature(
    State(state): State<Arc<AppState>>,
    Path(feature_raw): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let canonical = normalize_feature_name(&feature_raw).ok_or_else(|| {
        AppError::NotFound(format!("Automation feature '{feature_raw}' not found"))
    })?;
    let payload = parse_json_object(&body)?;
    let (eff_ts, _) = extract_request_timestamp(Some(&payload), None, &headers)?;
    let (is_active, config_opt) = parse_single_feature_update(&payload)?;
    let (raw_stype, raw_sid) = extract_scope(&query, Some(&payload));
    let scope = AutomationScope::parse(Some(&raw_stype), Some(&raw_sid))?;

    let st_clone = scope.scope_type.clone();
    let sid_clone = scope.scope_id.clone();
    let applied = state
        .db
        .run_blocking(move |db| {
            db.set_automation_config(
                canonical,
                is_active,
                config_opt.as_ref(),
                &st_clone,
                &sid_clone,
                Some(eff_ts),
            )
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    if applied {
        let is_sched = matches!(canonical, HEATING_SCHEDULE | ADAPTIVE_START);
        trigger_rules_evaluation(&state, &scope.scope_type, &scope.scope_id, is_sched).await;
    }
    let msg = if applied {
        "Feature updated"
    } else {
        "Stale feature update ignored"
    };
    Ok(api_success(
        json!({
            "status": "success",
            "feature": canonical,
            "scope": scope.scope_type,
            "scope_type": scope.scope_type,
            "scope_id": scope.scope_id,
            "ignored_stale": !applied,
        }),
        msg,
    ))
}

// =============================================================================
/// Resolves `(scope_type, scope_id)` from JSON body (priority) or URL query string.
fn extract_scope(
    query: &HashMap<String, String>,
    payload: Option<&Map<String, Value>>,
) -> (String, String) {
    let body_str = |k: &str| {
        payload?
            .get(k)?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let query_str = |k: &str| {
        query
            .get(k)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    let scope_type = body_str("scope_type")
        .or_else(|| body_str("scope"))
        .or_else(|| query_str("scope"))
        .or_else(|| query_str("scope_type"))
        .unwrap_or(SCOPE_GLOBAL)
        .to_owned();
    let scope_id = body_str("scope_id")
        .or_else(|| query_str("scope_id"))
        .unwrap_or(SCOPE_GLOBAL)
        .to_owned();
    (scope_type, scope_id)
}

// =============================================================================
/// Extracts and validates feature map entries from a batch `PUT /api/v1/automation` body.
fn extract_batch_features(payload: &Map<String, Value>) -> ApiResult<Map<String, Value>> {
    let mut features = Map::new();
    for (raw_name, val) in payload {
        if BATCH_META_KEYS.contains(&raw_name.as_str()) {
            continue;
        }
        let canon = normalize_feature_name(raw_name).ok_or_else(|| {
            AppError::Validation(format!("Unknown automation feature '{raw_name}'"))
        })?;
        if !val.is_object() {
            return Err(AppError::Validation(format!(
                "Configuration for '{raw_name}' must be an object"
            ))
            .into());
        }
        features.insert(canon.to_owned(), val.clone());
    }
    if features.is_empty() {
        return Err(AppError::Validation(
            "Payload must contain at least one automation feature".into(),
        )
        .into());
    }
    Ok(features)
}

// =============================================================================
/// Extracts `(is_active, config)` from a single-feature `PUT` body (including schedule shorthands).
fn parse_single_feature_update(
    payload: &Map<String, Value>,
) -> ApiResult<(Option<bool>, Option<Value>)> {
    let extract_bool = |m: &Map<String, Value>| {
        ACTIVE_KEYS
            .iter()
            .find_map(|k| m.get(*k).filter(|v| !v.is_null()))
            .map(|v| {
                v.as_bool()
                    .unwrap_or_else(|| v.as_i64().is_some_and(|n| n != 0))
            })
    };

    let mut is_active = extract_bool(payload);
    let mut config_opt = payload.get("config").cloned();

    if config_opt.is_none()
        && SCHEDULE_SHORTHAND_KEYS
            .iter()
            .any(|k| payload.contains_key(*k))
    {
        let sub: Map<String, Value> = payload
            .iter()
            .filter(|(k, _)| {
                !ACTIVE_KEYS.contains(&k.as_str()) && !BATCH_META_KEYS.contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        config_opt = Some(Value::Object(sub));
    }
    if is_active.is_none() {
        if let Some(cfg_obj) = config_opt.as_ref().and_then(Value::as_object) {
            is_active = extract_bool(cfg_obj);
        }
    }
    if is_active.is_none() && config_opt.is_none() {
        return Err(AppError::Validation(
            "Payload must contain 'is_active' and/or 'config'".into(),
        )
        .into());
    }
    Ok((is_active, config_opt))
}

// =============================================================================
/// Invalidates schedule cache when modified and triggers an immediate `RulesEngine` cycle.
async fn trigger_rules_evaluation(
    state: &AppState,
    scope_type: &str,
    scope_id: &str,
    invalidate_schedule: bool,
) {
    if let Some(ref engine) = state.rules_engine {
        if invalidate_schedule {
            engine.invalidate_schedule_scope(scope_type, scope_id).await;
        }
        engine.evaluate_now().await;
    } else {
        state.registry.set_heating_efficiency(None);
    }
}
