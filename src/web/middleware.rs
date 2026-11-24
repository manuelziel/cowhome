//! HTTP security headers, sanitized request logging, rate limiting, & `RFC 6750` auth middlewares.
//!
//! Enforces trusted-proxy client IP resolution before `RateLimiter`, rejects URL query
//! credentials (`?api_key=`), validates `Bearer` / `ApiKey` / `X-API-Key` in constant time,
//! and attaches `OWASP` security headers to every response.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use super::response::{ApiError, ApiResult};
use super::state::AppState;
use crate::core::config::AppConfig;
use crate::core::constants::{
    route_paths::{
        FORBIDDEN_QUERY_KEY_NAMES, HEADER_API_KEY, HEADER_AUTHORIZATION,
        HEADER_CONTENT_TYPE_OPTIONS, HEADER_FORWARDED_FOR, HEADER_REAL_IP,
        HEADER_VAL_CACHE_NO_STORE, HEADER_VAL_FRAME_DENY, HEADER_VAL_NO_REFERRER,
        HEADER_VAL_NOSNIFF,
    },
    service_defaults::RATE_LIMIT_MINUTE_WINDOW_SECS as MINUTE_WINDOW_SECS,
};
use crate::core::error::AppError;
use crate::core::http_security::{extract_request_api_key, resolve_client_ip, validate_api_key};
use crate::core::validation::{is_api_key_secure, sanitize_log_uri};

// =============================================================================
/// Outer middleware attaching `OWASP` security headers and logging sanitized request URIs.
pub async fn security_headers_and_logging(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = req.method().to_string();
    let raw_uri = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_owned(), |pq| pq.as_str().to_owned());
    let safe_uri = sanitize_log_uri(&raw_uri);

    let mut resp = next.run(req).await;
    attach_security_headers(resp.headers_mut());

    let code = resp.status().as_u16();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let line = format!("{method} {safe_uri} -> {code} ({ms:.1}ms)");
    if code >= 500 {
        state.logger.error(&line);
    } else if code >= 400 {
        state.logger.warning(&line);
    } else {
        state.logger.debug(&line);
    }
    resp
}

// =============================================================================
/// Middleware guarding `GET /api/v1/docs` with IP rate limits and server-key health check.
///
/// # Errors
/// Returns [`ApiError`] (`429` or `401`) if rate-limited or server `api_key` is unconfigured.
pub async fn docs_guard_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> ApiResult<Response> {
    let cfg = state.config_snapshot();
    let client_ip = extract_client_ip(&req, &cfg.api.trusted_proxies);
    enforce_rate_limits(&state, &cfg, client_ip)?;

    if !is_api_key_secure(&cfg.api.api_key) {
        state
            .logger
            .error("Docs UI rejected: missing or insecure default API key in config!");
        return Err(AppError::Auth(
            "Server configuration error: insecure or missing API key".into(),
        )
        .into());
    }
    Ok(next.run(req).await)
}

// =============================================================================
/// Middleware enforcing `RateLimiter`, query-secret rejection, and header-only API key auth.
///
/// # Errors
/// Returns [`ApiError`] (`401 Unauthorized` or `429 Too Many Requests`) on auth/rate failure.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> ApiResult<Response> {
    let cfg = state.config_snapshot();
    let client_ip = extract_client_ip(&req, &cfg.api.trusted_proxies);
    enforce_rate_limits(&state, &cfg, client_ip)?;

    if !is_api_key_secure(&cfg.api.api_key) {
        state
            .logger
            .error("API authentication rejected: missing or insecure default API key in config!");
        return Err(AppError::Auth(
            "Server configuration error: insecure or missing API key".into(),
        )
        .into());
    }

    if has_query_secret(req.uri().query()) {
        state.logger.warning(&format!(
            "Rejected URL query API key from [{client_ip}] (RFC 6750 header-only policy)"
        ));
        record_auth_outcome(&state, &cfg, client_ip, false)?;
        return Err(AppError::Auth("Invalid or missing API key".into()).into());
    }

    let auth_hdr = header_str(req.headers(), HEADER_AUTHORIZATION);
    let x_key_hdr = header_str(req.headers(), HEADER_API_KEY);
    let provided = extract_request_api_key(auth_hdr, x_key_hdr);

    match validate_api_key(provided, &cfg.api.api_key) {
        Ok(()) => {
            record_auth_outcome(&state, &cfg, client_ip, true)?;
            Ok(next.run(req).await)
        }
        Err(err) => {
            state.logger.warning(&format!(
                "Unauthorized access attempt from [{client_ip}]: {err}"
            ));
            record_auth_outcome(&state, &cfg, client_ip, false)?;
            Err(AppError::Auth("Invalid or missing API key".into()).into())
        }
    }
}

// =============================================================================
/// Attaches `OWASP` anti-framing, anti-sniffing, referrer, and `no-store` cache headers.
pub fn attach_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static(HEADER_VAL_FRAME_DENY),
    );
    headers.insert(
        HEADER_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static(HEADER_VAL_NOSNIFF),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static(HEADER_VAL_NO_REFERRER),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(HEADER_VAL_CACHE_NO_STORE),
    );
}

// =============================================================================
/// Resolves originating client IP honoring `ConnectInfo<SocketAddr>` and `trusted_proxies`.
fn extract_client_ip(req: &Request, trusted_proxies: &[String]) -> IpAddr {
    let remote_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |ci| ci.0.ip());
    let xff = header_str(req.headers(), HEADER_FORWARDED_FOR);
    let x_real = header_str(req.headers(), HEADER_REAL_IP);
    resolve_client_ip(remote_ip, xff, x_real, trusted_proxies)
}

// =============================================================================
/// Enforces global per-IP request throttling (`120/min`) and failed-login lockout (`5/h`).
fn enforce_rate_limits(state: &AppState, cfg: &AppConfig, client_ip: IpAddr) -> ApiResult<()> {
    if !cfg.rate_limit.enabled {
        return Ok(());
    }
    let ip_key = format!("ip:{client_ip}");
    if state.rate_limiter.is_rate_limited(
        &ip_key,
        cfg.rate_limit.max_requests_per_minute_per_ip,
        MINUTE_WINDOW_SECS,
    ) {
        return Err(ApiError::rate_limited(
            "Too many requests. Please try again later.",
            MINUTE_WINDOW_SECS,
        ));
    }
    let login_key = format!("login:{client_ip}");
    if state.rate_limiter.check_only(
        &login_key,
        cfg.rate_limit.max_login_attempts,
        cfg.rate_limit.window,
    ) {
        return Err(ApiError::rate_limited(
            "Too many failed authentication attempts. Please try again later.",
            cfg.rate_limit.window,
        ));
    }
    Ok(())
}

// =============================================================================
/// Updates brute-force `RateLimiter` counters after evaluating an authentication attempt.
fn record_auth_outcome(
    state: &AppState,
    cfg: &AppConfig,
    client_ip: IpAddr,
    success: bool,
) -> ApiResult<()> {
    if !cfg.rate_limit.enabled {
        return Ok(());
    }
    let login_key = format!("login:{client_ip}");
    if success {
        state.rate_limiter.reset(&login_key);
        return Ok(());
    }
    state.rate_limiter.record(&login_key, cfg.rate_limit.window);
    if state.rate_limiter.check_only(
        &login_key,
        cfg.rate_limit.max_login_attempts,
        cfg.rate_limit.window,
    ) {
        return Err(ApiError::rate_limited(
            "Too many failed authentication attempts. Please try again later.",
            cfg.rate_limit.window,
        ));
    }
    Ok(())
}

// =============================================================================
/// Checks whether raw URL `query` contains any forbidden credential parameter (`?api_key=`).
fn has_query_secret(query: Option<&str>) -> bool {
    let Some(q) = query else {
        return false;
    };
    q.split('&').any(|pair| {
        let key = pair.split_once('=').map_or(pair, |(k, _)| k).trim();
        FORBIDDEN_QUERY_KEY_NAMES
            .iter()
            .any(|bad| key.eq_ignore_ascii_case(bad))
    })
}

// =============================================================================
/// Reads a trimmed UTF-8 header value by `name`.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
