//! State, Weather, History, & OpenAPI/Swagger documentation HTTP handlers.
//!
//! Serves `GET /api/v1/state`, `GET /api/v1/weather`, `GET /api/v1/history`,
//! `GET /api/v1/docs`, and `GET /api/v1/openapi.yaml`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Value, json};

use crate::core::constants::embedded_assets::{OPENAPI_SPEC_YAML, SWAGGER_UI_HTML};
use crate::core::domain_constants::{
    analytics::history_defaults::{DEFAULT_LIMIT, DEFAULT_RANGE, MAX_LIMIT},
    automation::automation_defaults::{HUMIDITY_HYSTERESIS_PERCENT, HUMIDITY_THRESHOLD},
};
use crate::core::energy_analytics::EnergySummary;
use crate::core::error::AppError;
use crate::core::psychrometrics::{DevicePsychrometricsInput, compute_device_psychrometrics};
use crate::db::efficiency::AnalyticsContext;
use crate::db::history::HistoryQuery;
use crate::web::response::{ApiError, ApiResult, api_success};
use crate::web::state::AppState;

// =============================================================================
/// `GET /api/v1/state` — returns the complete Digital Twin, weather, efficiency, & energy summary.
///
/// # Errors
/// Returns [`ApiError`] if background SQLite cache population fails.
pub async fn get_state(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let weather = state
        .weather_service
        .as_ref()
        .and_then(|ws| ws.get_current_weather());
    let (temp_out, abs_out) = weather
        .as_ref()
        .map_or((None, None), |cw| cw.outdoor_psychrometrics());

    enrich_missing_device_psychrometrics(&state, temp_out, abs_out);
    let devices = state.registry.get_all_devices();
    let (efficiency, energy_summary) = resolve_house_summaries(&state).await?;
    let forecast = efficiency
        .as_ref()
        .and_then(|e| e.get("heating_forecast"))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(api_success(
        json!({
            "devices": devices,
            "weather": weather,
            "heating_efficiency": efficiency,
            "heating_forecast": forecast,
            "energy_summary": energy_summary,
        }),
        "success",
    ))
}

// =============================================================================
/// Populates missing `DIN 4108-2` psychrometrics on humidity-bearing Digital Twin devices.
fn enrich_missing_device_psychrometrics(
    state: &AppState,
    temp_out: Option<f64>,
    abs_out: Option<f64>,
) {
    for dev in state.registry.get_all_devices() {
        if dev.humidity.is_some() && dev.dew_point_c.is_none() {
            let temp_in = dev
                .as_thermostat()
                .map(|t| t.temperature_current)
                .or_else(|| dev.as_sensor().and_then(|s| s.temperature_current));
            let psych = compute_device_psychrometrics(&DevicePsychrometricsInput {
                temp_in_c: temp_in,
                rel_humidity_in_pct: dev.humidity,
                fallback_target_c: dev.as_thermostat().map(|t| t.temperature_target),
                temp_base_c: None,
                temp_out_c: temp_out,
                abs_out_gm3: abs_out,
                humidity_threshold: HUMIDITY_THRESHOLD,
                hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
                guard_active: dev.humidity_guard_active,
                rule_active: false,
            });
            state
                .registry
                .mutate_device(&dev.ain, |d| d.apply_psychrometrics(psych));
        }
    }
}

// =============================================================================
/// Reads cached house efficiency & energy summaries or populates them once from SQLite.
///
/// # Errors
/// Returns [`ApiError`] if the blocking SQLite task fails.
async fn resolve_house_summaries(
    state: &AppState,
) -> ApiResult<(Option<Value>, Option<EnergySummary>)> {
    let mut efficiency = state.registry.get_heating_efficiency();
    let mut energy_summary = state.registry.get_energy_summary();
    if efficiency.is_some() && energy_summary.is_some() {
        return Ok((efficiency, energy_summary));
    }
    let (daily_fc, live_out) = state.forecast_and_outdoor();
    let tz_name = state.home_tz_name();
    let live_targets = state.registry.get_live_thermostat_targets();
    let live_switches = state.registry.get_live_switch_states();
    let (need_eff, need_en) = (efficiency.is_none(), energy_summary.is_none());

    let (db_eff, db_en) = state
        .db
        .run_blocking(move |db| {
            let eff = need_eff
                .then(|| {
                    let ctx = AnalyticsContext {
                        since: None,
                        range_str: None,
                        daily_forecast: &daily_fc,
                        live_targets: Some(&live_targets),
                        live_outdoor_temp: live_out,
                    };
                    db.get_house_heating_efficiency(&ctx)
                })
                .flatten();
            let en = need_en
                .then(|| db.get_house_energy_summary(Some(&tz_name), None, Some(&live_switches)))
                .flatten();
            Ok((eff, en))
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;
    if let Some(eff) = db_eff {
        state.registry.set_heating_efficiency(Some(eff.clone()));
        efficiency = Some(eff);
    }
    if let Some(en) = db_en {
        state.registry.set_energy_summary(Some(en.clone()));
        energy_summary = Some(en);
    }
    Ok((efficiency, energy_summary))
}

// =============================================================================
/// `GET /api/v1/weather` — returns cached current weather, 2h hourly slots, & 5-day forecast.
///
/// # Errors
/// Returns [`ApiError`] (`503 Service Unavailable`) if `weather_service` is not configured.
pub async fn get_weather(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let Some(ref ws) = state.weather_service else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({
                "status": "error",
                "message": "Weather service is not configured or available",
                "code": StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                "data": Value::Null,
            })),
        )
            .into_response());
    };
    let snap = ws.get_snapshot();
    Ok(api_success(
        serde_json::to_value(snap).unwrap_or_else(|_| json!({})),
        "success",
    ))
}

// =============================================================================
/// `GET /api/v1/history` — returns column-oriented time-series history and inline analytics.
///
/// # Errors
/// Returns [`ApiError`] (`400` / `404` / `503`) on invalid query params or unknown device/group.
pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let device = params
        .get("device")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Validation("Missing required query parameter: 'device'".into()))?
        .to_owned();

    let since: Option<i64> = match params.get("since") {
        Some(raw) => {
            let parsed = raw.trim().parse::<i64>().map_err(|_| {
                AppError::Validation(
                    "Query parameter 'since' must be a non-negative integer UNIX timestamp".into(),
                )
            })?;
            if parsed < 0 {
                return Err(AppError::Validation(
                    "Query parameter 'since' must be a non-negative integer UNIX timestamp".into(),
                )
                .into());
            }
            Some(parsed)
        }
        None => None,
    };

    let limit: usize = match params.get("limit") {
        Some(raw) => {
            let parsed = raw.trim().parse::<usize>().map_err(|_| {
                AppError::Validation(format!(
                    "Query parameter 'limit' must be between 0 and {MAX_LIMIT} (0 = unlimited)"
                ))
            })?;
            if parsed > MAX_LIMIT {
                return Err(AppError::Validation(format!(
                    "Query parameter 'limit' must be between 0 and {MAX_LIMIT} (0 = unlimited)"
                ))
                .into());
            }
            parsed
        }
        None => DEFAULT_LIMIT,
    };

    let range_val = params
        .get("range")
        .map_or_else(|| DEFAULT_RANGE.to_owned(), |s| s.trim().to_owned());
    let (daily_fc, live_out) = state.forecast_and_outdoor();
    let tz_name = state.home_tz_name();
    let live_targets = state.registry.get_live_thermostat_targets();
    let live_switches = state.registry.get_live_switch_states();

    let payload = state
        .db
        .run_blocking(move |db| {
            let q = HistoryQuery {
                ain: &device,
                since,
                range_str: Some(&range_val),
                limit,
                daily_forecast: &daily_fc,
                live_targets: Some(&live_targets),
                tz_name: Some(&tz_name),
                live_switches: Some(&live_switches),
                live_outdoor_temp: live_out,
            };
            db.get_device_history(&q)
        })
        .await
        .map_err(|e| ApiError::logged(e, &state.logger))?;

    Ok(api_success(payload, "Historical data retrieved"))
}

// =============================================================================
/// `GET /api/v1/docs` — serves the zero-secret `sessionStorage`-gated Swagger UI HTML shell.
pub async fn get_docs_ui() -> Response {
    Html(SWAGGER_UI_HTML).into_response()
}

// =============================================================================
/// `GET /api/v1/openapi.yaml` — serves the header-authenticated OpenAPI 3.0.3 YAML specification.
pub async fn get_openapi_spec() -> Response {
    let mut resp = OPENAPI_SPEC_YAML.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/yaml; charset=utf-8"),
    );
    resp
}
