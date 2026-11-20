//! Standardized JSON response envelopes, `IntoResponse` error mapping, & body helpers.
//!
//! Guarantees 100% wire compatibility with `assets/openapi.yaml` (`{"status": "success"|"error", ...}`),
//! `RFC 6750` `WWW-Authenticate` challenges on `401`, and `RFC 6585` `Retry-After` headers on `429`.

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::core::constants::{
    route_paths::{HEADER_CLIENT_TIMESTAMP, HEADER_WWW_AUTHENTICATE, WWW_AUTH_BEARER_REALM},
    service_defaults::{DEFAULT_RATE_LIMIT_WINDOW_SECS, RATE_LIMIT_MINUTE_WINDOW_SECS},
};
use crate::core::error::AppError;
use crate::core::logger::Logger;
use crate::core::time_util::normalize_client_timestamp;

/// Candidate body/param field names carrying an explicit client LWW timestamp.
const TIMESTAMP_KEYS: &[&str] = &["client_timestamp", "timestamp"];

// =============================================================================
/// Standardized HTTP error wrapper converting [`AppError`] into CowHome JSON envelopes.
#[derive(Debug)]
pub struct ApiError {
    /// Underlying domain error.
    pub err: AppError,
    /// Optional `Retry-After` window in seconds for `429 Too Many Requests`.
    pub retry_after_secs: Option<u64>,
}

/// Handler return alias across `src/web/handlers/`.
pub type ApiResult<T = Response> = Result<T, ApiError>;

impl ApiError {
    /// Creates an [`ApiError`] without custom `Retry-After` override.
    #[must_use]
    pub const fn new(err: AppError) -> Self {
        Self {
            err,
            retry_after_secs: None,
        }
    }

    /// Creates a `429 Too Many Requests` [`ApiError`] with an explicit `Retry-After` header.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>, retry_after_secs: u64) -> Self {
        Self {
            err: AppError::RateLimit(message.into()),
            retry_after_secs: Some(retry_after_secs),
        }
    }

    /// Logs backend `503`/`500` errors via `logger` and returns `Self` for `?` propagation.
    #[must_use]
    pub fn logged(err: AppError, logger: &Logger) -> Self {
        if matches!(
            err,
            AppError::Database(_) | AppError::Service(_) | AppError::Io { .. }
        ) {
            logger.error(&format!("Backend error: {err}"));
        }
        Self::new(err)
    }
}

impl From<AppError> for ApiError {
    /// Wraps an [`AppError`] into an HTTP [`ApiError`].
    fn from(err: AppError) -> Self {
        Self::new(err)
    }
}

impl IntoResponse for ApiError {
    /// Serializes [`ApiError`] into CowHome's standardized JSON envelope with `RFC 6750` / `Retry-After` headers.
    fn into_response(self) -> Response {
        let (status, message) = match &self.err {
            AppError::Auth(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::RateLimit(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Validation(msg) | AppError::Config(msg) | AppError::Usage(msg) => {
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            AppError::Database(_) | AppError::Service(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                String::from("A backend service is currently unavailable"),
            ),
            AppError::Security(_)
            | AppError::Setup(_)
            | AppError::Io { .. }
            | AppError::Cancelled => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("Internal Server Error"),
            ),
        };

        let body = json!({
            "status": "error",
            "message": message,
            "code": status.as_u16(),
            "data": Value::Null,
        });
        let mut resp = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            if let Ok(val) = HeaderValue::from_str(WWW_AUTH_BEARER_REALM) {
                resp.headers_mut().insert(HEADER_WWW_AUTHENTICATE, val);
            }
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            let secs = self.retry_after_secs.unwrap_or_else(|| {
                if message.to_ascii_lowercase().contains("authentication") {
                    DEFAULT_RATE_LIMIT_WINDOW_SECS
                } else {
                    RATE_LIMIT_MINUTE_WINDOW_SECS
                }
            });
            if let Ok(val) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, val);
            }
        }
        resp
    }
}

// =============================================================================
/// Builds a `200 OK` standardized CowHome JSON envelope (`{"status": "success", ...}`).
#[must_use]
pub fn api_success(data: Value, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "status": "success",
            "message": message,
            "data": data,
        })),
    )
        .into_response()
}

// =============================================================================
/// Builds a `201 Created` standardized CowHome JSON envelope.
#[must_use]
pub fn api_created(data: Value, message: &str) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({
            "status": "success",
            "message": message,
            "data": data,
        })),
    )
        .into_response()
}

// =============================================================================
/// Parses raw HTTP request `bytes` as a JSON object (`{}` when body is empty).
///
/// # Errors
/// Returns [`ApiError`] (`400 Bad Request`) if `bytes` is malformed JSON or not an object.
pub fn parse_json_object(bytes: &Bytes) -> ApiResult<Map<String, Value>> {
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    let parsed: Value = serde_json::from_slice(bytes)
        .map_err(|err| AppError::Validation(format!("Invalid JSON payload: {err}")))?;
    let Value::Object(map) = parsed else {
        return Err(AppError::Validation("JSON request body must be an object".into()).into());
    };
    Ok(map)
}

// =============================================================================
/// Extracts `(effective_timestamp_secs, is_explicit)` from `payload`, `params`, or `X-Client-Timestamp`.
///
/// # Errors
/// Returns [`ApiError`] (`400 Bad Request`) if a provided timestamp is non-numeric or `<= 0`.
pub fn extract_request_timestamp(
    payload: Option<&Map<String, Value>>,
    params: Option<&Map<String, Value>>,
    headers: &HeaderMap,
) -> ApiResult<(f64, bool)> {
    let mut body_val: Option<&Value> = None;
    for src in [payload, params].into_iter().flatten() {
        for &key in TIMESTAMP_KEYS {
            if let Some(v) = src.get(key).filter(|v| !v.is_null()) {
                body_val = Some(v);
                break;
            }
        }
        if body_val.is_some() {
            break;
        }
    }
    let hdr_val = headers
        .get(HEADER_CLIENT_TIMESTAMP)
        .and_then(|h| h.to_str().ok());
    Ok(normalize_client_timestamp(body_val, hdr_val, None)?)
}
