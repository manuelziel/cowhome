//! System, Router TR-064, Storage, Energy Tariff, & Universal Diagnostics HTTP handlers.
//!
//! Serves `/api/v1/system/status`, `/hosts`, `/calls`, `/storage`, `/energy` (`/settings`),
//! and `/diagnostics` (`/ai-diagnostics`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::core::constants::diagnostics_defaults::{
    DEFAULT_LOG_LIMIT, DEFAULT_RANGE, MAX_LOG_LIMIT,
};
use crate::core::diagnostics::{validate_diagnostics_range, DiagnosticsQuery};
use crate::core::domain_constants::{
    analytics::energy_defaults::{
        BASE_FEE_PAYLOAD_KEYS, CURRENCY, PRICE_PAYLOAD_KEYS, SETTING_KEY_BASE_FEE,
        SETTING_KEY_PRICE,
    },
    hardware::tr064_defaults::DEFAULT_CALL_LIMIT,
};
use crate::core::error::AppError;
use crate::services::diagnostics::DiagnosticsService;
use crate::web::response::{
    api_success, extract_request_timestamp, parse_json_object, ApiError, ApiResult,
};
use crate::web::state::AppState;

// =============================================================================
/// `GET /api/v1/system/status` — fetches FritzBox TR-064 device info, WAN IP, & WLAN status.
///
/// # Errors
/// Infallible partial queries degrade gracefully to defaults matching Python `SystemStatusHandler`.
pub async fn get_system_status(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let router_info = state
        .tr064
        .get_device_info()
        .await
        .and_then(|i| serde_json::to_value(i).ok())
        .unwrap_or_else(|| json!({}));
    let has_info = router_info.as_object().is_some_and(|m| !m.is_empty());

    let mut conn = match state.tr064.get_system_status().await {
        Ok(st) => serde_json::to_value(st).unwrap_or_else(|_| json!({})),
        Err(_) => json!({
            "is_connected": has_info,
            "is_linked": has_info,
            "uptime": 0,
            "external_ip": Value::Null,
            "bytes_sent": 0,
            "bytes_received": 0,
        }),
    };
    if has_info
        && !conn
            .get("is_connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        if let Some(obj) = conn.as_object_mut() {
            obj.insert(String::from("is_connected"), json!(true));
        }
    }
    let mut wan_ip = conn
        .get("external_ip")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if wan_ip.is_none() {
        wan_ip = state.tr064.get_external_ip().await;
    }
    if let (Some(ref ip), Some(obj)) = (&wan_ip, conn.as_object_mut()) {
        if obj.get("external_ip").is_none_or(Value::is_null) {
            obj.insert(String::from("external_ip"), json!(ip));
        }
    }

    let wlan_main = state
        .tr064
        .get_wlan_status()
        .await
        .ok()
        .and_then(|w| serde_json::to_value(w).ok())
        .unwrap_or_else(|| json!({ "enabled": false }));
    let wlan_guest = serde_json::to_value(state.tr064.get_guest_wlan_status().await)
        .unwrap_or_else(|_| json!({ "enabled": false }));

    Ok(api_success(
        json!({
            "router_info": router_info,
            "wan_ip": wan_ip,
            "connection_status": conn,
            "wlan_main": wlan_main,
            "wlan_guest": wlan_guest,
        }),
        "Router status retrieved",
    ))
}

// =============================================================================
/// `GET /api/v1/system/hosts` — returns LAN/WLAN hosts connected to the FritzBox.
///
/// # Errors
/// Returns [`ApiError`] (`503 Service Unavailable`) if TR-064 host enumeration fails.
pub async fn get_system_hosts(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let hosts = state
        .tr064
        .get_connected_hosts()
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    Ok(api_success(
        json!({ "hosts": hosts }),
        "Connected hosts retrieved",
    ))
}

// =============================================================================
/// `GET /api/v1/system/calls` — returns recent FritzBox call list and answering-machine state.
///
/// # Errors
/// Returns [`ApiError`] (`503 Service Unavailable`) if TR-064 telephony query fails.
pub async fn get_system_calls(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let calls = state.tr064.get_call_list(DEFAULT_CALL_LIMIT).await;
    let tam = state.tr064.get_answering_machine_status().await;
    Ok(api_success(
        json!({
            "calls": calls,
            "answering_machine": tam,
        }),
        "Call log retrieved",
    ))
}

// =============================================================================
/// `GET /api/v1/system/storage` — returns SQLite `.db`/`-wal`/`-shm` & host `statvfs` metrics.
///
/// # Errors
/// Returns [`ApiError`] (`503 Service Unavailable`) if querying SQLite storage fails.
pub async fn get_system_storage(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let stats = state
        .db
        .run_blocking(|db| db.get_storage_stats())
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    Ok(api_success(stats, "Storage diagnostics retrieved"))
}

// =============================================================================
/// `GET /api/v1/system/energy` (`/settings`) — returns electricity tariff, base fee, & summary.
///
/// # Errors
/// Returns [`ApiError`] if SQLite fails.
pub async fn get_system_energy(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let cached_summary = state.registry.get_energy_summary();
    let need_summary = cached_summary.is_none();
    let tz_name = state.home_tz_name();
    let live_sw = state.registry.get_live_switch_states();

    let (price, base_fee, fresh_summary) = state
        .db
        .run_blocking(move |db| {
            let p = db.get_electricity_price();
            let b = db.get_electricity_base_fee();
            let s = need_summary
                .then(|| db.get_house_energy_summary(Some(&tz_name), None, Some(&live_sw)))
                .flatten();
            Ok((p, b, s))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    let summary = match fresh_summary {
        Some(s) => {
            state.registry.set_energy_summary(Some(s.clone()));
            Some(s)
        }
        None => cached_summary,
    };

    Ok(api_success(
        json!({
            SETTING_KEY_PRICE: price,
            SETTING_KEY_BASE_FEE: base_fee,
            "currency": CURRENCY,
            "energy_summary": summary,
        }),
        "Energy settings retrieved",
    ))
}

// =============================================================================
/// `PUT /api/v1/system/energy` (`/settings`) — updates tariff (`EUR/kWh`) & base fee (`EUR/month`).
///
/// # Errors
/// Returns [`ApiError`] (`400 Bad Request`) if payload omits both fields or fails bounds checks.
pub async fn put_system_energy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let payload = parse_json_object(&body)?;
    let (raw_price, raw_base) = extract_energy_values(&payload)?;
    let (eff_ts, is_explicit) = extract_request_timestamp(Some(&payload), None, &headers)?;
    let ts_opt = is_explicit.then_some(eff_ts);
    let tz_name = state.home_tz_name();
    let live_sw = state.registry.get_live_switch_states();

    let (price, base_fee, any_applied, summary) = state
        .db
        .run_blocking(move |db| {
            let mut applied = Vec::new();
            let mut p = db.get_electricity_price();
            let mut b = db.get_electricity_base_fee();
            if let Some(ref rp) = raw_price {
                let (np, app) = db.set_electricity_price(rp, ts_opt)?;
                p = np;
                applied.push(app);
            }
            if let Some(ref rb) = raw_base {
                let (nb, app) = db.set_electricity_base_fee(rb, ts_opt)?;
                b = nb;
                applied.push(app);
            }
            let sum = db.get_house_energy_summary(Some(&tz_name), None, Some(&live_sw));
            Ok((p, b, applied.into_iter().any(|x| x), sum))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    state.registry.set_energy_summary(summary.clone());
    let ignored_stale = !any_applied;
    let msg = if ignored_stale {
        "Stale energy setting ignored"
    } else {
        "Energy settings updated"
    };

    Ok(api_success(
        json!({
            SETTING_KEY_PRICE: price,
            SETTING_KEY_BASE_FEE: base_fee,
            "currency": CURRENCY,
            "ignored_stale": ignored_stale,
            "energy_summary": summary,
        }),
        msg,
    ))
}

// =============================================================================
/// `GET /api/v1/system/diagnostics` (`/ai-diagnostics`) — Universal Diagnostics Schema v1.0.
///
/// # Errors
/// Returns [`ApiError`] (`400 Bad Request`) if `range` or `limit` query parameters are invalid.
pub async fn get_system_diagnostics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let range_raw = params.get("range").map_or(DEFAULT_RANGE, String::as_str);
    let range_str = validate_diagnostics_range(Some(range_raw))?.to_owned();

    let log_limit = match params.get("limit") {
        None => DEFAULT_LOG_LIMIT,
        Some(raw) => {
            let parsed = raw.trim().parse::<usize>().map_err(|_| {
                AppError::Validation("Parameter 'limit' must be an integer.".into())
            })?;
            if !(1..=MAX_LOG_LIMIT).contains(&parsed) {
                return Err(AppError::Validation(format!(
                    "Parameter 'limit' must be between 1 and {MAX_LOG_LIMIT}."
                ))
                .into());
            }
            parsed
        }
    };
    let device = params
        .get("device")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let collector = DiagnosticsService::new(
        state.config_snapshot(),
        Some(state.db.clone()),
        Some(Arc::clone(&state.registry)),
        state.rules_engine.clone(),
        state.weather_service.clone(),
        Some(Arc::clone(&state.rate_limiter)),
        state.logger.clone(),
    );
    let report = collector
        .collect_report_async(DiagnosticsQuery {
            range: Some(range_str),
            limit: Some(log_limit),
            device,
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    Ok(api_success(report, "System diagnostics report generated"))
}

// =============================================================================
/// Extracts optional `(price_val, base_fee_val)` from `PUT /api/v1/system/energy`.
fn extract_energy_values(
    payload: &Map<String, Value>,
) -> ApiResult<(Option<Value>, Option<Value>)> {
    let raw_price = PRICE_PAYLOAD_KEYS
        .iter()
        .find_map(|k| payload.get(*k).filter(|v| !v.is_null()).cloned());
    let raw_base = BASE_FEE_PAYLOAD_KEYS
        .iter()
        .find_map(|k| payload.get(*k).filter(|v| !v.is_null()).cloned());
    if raw_price.is_none() && raw_base.is_none() {
        return Err(AppError::Validation(format!(
            "Missing required field: '{SETTING_KEY_PRICE}' or '{SETTING_KEY_BASE_FEE}'"
        ))
        .into());
    }
    Ok((raw_price, raw_base))
}
