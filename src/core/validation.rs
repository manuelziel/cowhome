//! Pure input validation, constant-time secret comparison, and log sanitization helpers.
//!
//! Implements stateless (`pure`) functions enforcing `RFC 1035`, `RFC 4291`, `OWASP ASVS v4.0`,
//! `IANA` timezone validation, and `NIST SP 800-63B` constant-time equality without I/O.

use std::net::{IpAddr, Ipv6Addr};
use std::path::{Component, Path};

use chrono_tz::Tz;
use subtle::ConstantTimeEq;

use crate::core::constants::service_defaults::{
    MASKED_SECRET_PREFIX, MASKED_SECRET_SUFFIX_LEN, MIN_API_KEY_LEN, REDACTED_VALUE,
    WEAK_PLACEHOLDER_KEYS,
};
use crate::core::domain_constants::automation::weather_defaults::TIMEZONE_AUTO;
use crate::core::error::{AppError, AppResult};

/// Maximum total length of an RFC 1035 domain name.
const MAX_HOSTNAME_LEN: usize = 253;

/// Maximum length of a single RFC 1035 DNS label.
const MAX_DNS_LABEL_LEN: usize = 63;

// =============================================================================
/// Performs a constant-time byte slice comparison (`subtle::ConstantTimeEq`).
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.ct_eq(right).into()
}

// =============================================================================
/// Checks whether `api_key` meets `OWASP ASVS v4.0` length, printable ASCII, and placeholder rules.
#[must_use]
pub fn is_api_key_secure(api_key: &str) -> bool {
    let trimmed = api_key.trim();
    if trimmed.len() < MIN_API_KEY_LEN || trimmed != api_key {
        return false;
    }
    if !trimmed.chars().all(|ch| ch.is_ascii_graphic()) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !WEAK_PLACEHOLDER_KEYS.contains(&lower.as_str())
}

// =============================================================================
/// Masks a secret credential for safe terminal display (`••••••••abcd` or `(not set)`).
#[must_use]
pub fn mask_secret(secret: &str) -> String {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return String::from("(not set)");
    }
    let char_count = trimmed.chars().count();
    if char_count <= MASKED_SECRET_SUFFIX_LEN * 2 {
        return String::from(MASKED_SECRET_PREFIX);
    }
    let suffix: String = trimmed
        .chars()
        .skip(char_count - MASKED_SECRET_SUFFIX_LEN)
        .collect();
    format!("{MASKED_SECRET_PREFIX}{suffix}")
}

// =============================================================================
/// Redacts every occurrence of `secret` inside `message` before logging.
#[must_use]
pub fn redact_text(message: &str, secret: Option<&str>) -> String {
    match secret.map(str::trim) {
        Some(token) if !token.is_empty() && message.contains(token) => {
            message.replace(token, REDACTED_VALUE)
        }
        _ => message.to_owned(),
    }
}

// =============================================================================
/// Redacts sensitive query-string parameters (`?api_key=...`, `&token=...`) inside log text.
///
/// Stops each parameter value cleanly at URI boundaries (whitespace, `#`, quotes, or `&`)
/// so trailing prose in log messages is preserved verbatim.
#[must_use]
pub fn sanitize_log_uri(raw_text: &str) -> String {
    if !raw_text.contains('?') {
        return raw_text.to_owned();
    }
    let mut out = String::with_capacity(raw_text.len());
    let mut rest = raw_text;

    while let Some(q_idx) = rest.find('?') {
        out.push_str(&rest[..=q_idx]);
        let after_q = &rest[q_idx + 1..];
        let query_end = after_q.find(is_uri_boundary_char).unwrap_or(after_q.len());
        redact_query_segment(&after_q[..query_end], &mut out);
        rest = &after_q[query_end..];
    }
    out.push_str(rest);
    out
}

// =============================================================================
/// Rewrites `k=v&k2=v2` into `out`, replacing sensitive parameter values with `**REDACTED**`.
fn redact_query_segment(query: &str, out: &mut String) {
    for (idx, pair) in query.split('&').enumerate() {
        if idx > 0 {
            out.push('&');
        }
        match pair.split_once('=') {
            Some((k, _)) if is_sensitive_query_param(k) => {
                out.push_str(k);
                out.push('=');
                out.push_str(REDACTED_VALUE);
            }
            _ => out.push_str(pair),
        }
    }
}

// =============================================================================
/// Returns `true` if `ch` terminates a URI query component inside a log message.
fn is_uri_boundary_char(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '#' | '"' | '\'' | ')' | ']' | '>')
}

// =============================================================================
/// Returns `true` if a URL query parameter name may carry authentication material.
fn is_sensitive_query_param(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "api_key"
            | "apikey"
            | "api_secret"
            | "token"
            | "access_token"
            | "secret"
            | "password"
            | "passwd"
            | "pass"
            | "key"
            | "authorization"
            | "sid"
            | "response"
    )
}

// =============================================================================
/// Parses an IP literal or `IP/prefix` CIDR notation into `(IpAddr, prefix_bits)`.
///
/// # Errors
/// Returns [`AppError::Validation`] if the IP literal or prefix length is malformed.
pub fn parse_ip_or_cidr(entry: &str) -> AppResult<(IpAddr, u8)> {
    let trimmed = entry.trim();
    if let Some((ip_part, prefix_part)) = trimmed.split_once('/') {
        let ip: IpAddr = ip_part
            .parse()
            .map_err(|_| AppError::Validation(format!("Invalid CIDR IP address in '{entry}'")))?;
        let max_bits = if ip.is_ipv4() { 32u8 } else { 128u8 };
        let bits: u8 = prefix_part
            .parse()
            .map_err(|_| AppError::Validation(format!("Invalid CIDR prefix in '{entry}'")))?;
        if bits > max_bits {
            return Err(AppError::Validation(format!(
                "CIDR prefix out of range in '{entry}' (max {max_bits})"
            )));
        }
        return Ok((ip, bits));
    }
    let ip: IpAddr = trimmed.parse().map_err(|_| {
        AppError::Validation(format!(
            "Invalid IP or CIDR literal '{entry}' (expected IPv4, IPv6, or CIDR)"
        ))
    })?;
    let host_bits = if ip.is_ipv4() { 32 } else { 128 };
    Ok((ip, host_bits))
}

// =============================================================================
/// Validates a hostname (`RFC 1035`), `IPv4`, or `IPv6` (`RFC 4291` / bracketed `RFC 2732`).
///
/// # Errors
/// Returns [`AppError::Validation`] if `host` is empty, leading-dash, or malformed.
pub fn validate_host_or_ip(host: &str) -> AppResult<()> {
    if host.is_empty() || host != host.trim() || host.starts_with('-') {
        return Err(AppError::Validation(format!(
            "Invalid host or IP address '{host}'"
        )));
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return v6
            .parse::<Ipv6Addr>()
            .map(|_| ())
            .map_err(|_| AppError::Validation(format!("Invalid bracketed IPv6 literal '{host}'")));
    }
    if host.parse::<IpAddr>().is_ok() || is_valid_rfc1035_hostname(host) {
        return Ok(());
    }
    Err(AppError::Validation(format!(
        "Invalid host '{host}' (expected RFC 1035 hostname, IPv4, or IPv6)"
    )))
}

// =============================================================================
/// Checks whether `host` conforms to RFC 1035 / RFC 1123 DNS hostname syntax.
#[must_use]
pub fn is_valid_rfc1035_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > MAX_DNS_LABEL_LEN
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return false;
        }
    }
    !host.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

// =============================================================================
/// Validates an `http://` or `https://` base endpoint URL (`RFC 9110`).
///
/// # Errors
/// Returns [`AppError::Validation`] if the scheme, authority, or characters are invalid.
pub fn validate_http_endpoint(url: &str) -> AppResult<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed != url || trimmed.chars().any(char::is_whitespace) {
        return Err(AppError::Validation(
            "Base URL must not be empty or contain whitespace".into(),
        ));
    }
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| {
            AppError::Validation(format!(
                "URL '{url}' must start with 'https://' or 'http://'"
            ))
        })?;
    let authority = rest.split('/').next().unwrap_or("");
    let host_part = extract_url_host(authority)?;
    validate_host_or_ip(host_part)
}

// =============================================================================
/// Splits `host[:port]` or `[ipv6][:port]` authority into a validated host slice.
///
/// # Errors
/// Returns [`AppError::Validation`] if the host is missing, brackets are unclosed, or port is `0`.
fn extract_url_host(authority: &str) -> AppResult<&str> {
    if authority.is_empty() {
        return Err(AppError::Validation("URL is missing a host".into()));
    }
    if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| AppError::Validation("Unclosed IPv6 bracket in URL".into()))?;
        return Ok(&authority[..=end]);
    }
    if let Some((host, port_str)) = authority.rsplit_once(':') {
        let port: u16 = port_str
            .parse()
            .map_err(|_| AppError::Validation(format!("Invalid URL port '{port_str}'")))?;
        if port == 0 {
            return Err(AppError::Validation("URL port must not be 0".into()));
        }
        return Ok(host);
    }
    Ok(authority)
}

// =============================================================================
/// Validates WGS-84 geographic coordinates (`latitude` in `-90..=90`, `longitude` in `-180..=180`).
///
/// # Errors
/// Returns [`AppError::Validation`] if either coordinate is non-finite or out of bounds.
pub fn validate_coordinates(latitude: f64, longitude: f64) -> AppResult<()> {
    if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
        return Err(AppError::Validation(format!(
            "weather.latitude ({latitude}) must be between -90.0 and 90.0"
        )));
    }
    if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
        return Err(AppError::Validation(format!(
            "weather.longitude ({longitude}) must be between -180.0 and 180.0"
        )));
    }
    Ok(())
}

// =============================================================================
/// Validates `weather.timezone` as `"auto"` or a known `IANA` timezone (`Europe/Berlin`).
///
/// # Errors
/// Returns [`AppError::Validation`] if `timezone` is neither `"auto"` nor in the IANA tzdb.
pub fn validate_timezone(timezone: &str) -> AppResult<()> {
    let trimmed = timezone.trim();
    if trimmed.is_empty() || trimmed != timezone {
        return Err(AppError::Validation(
            "weather.timezone must not be empty or untrimmed".into(),
        ));
    }
    if trimmed.eq_ignore_ascii_case(TIMEZONE_AUTO) || trimmed.parse::<Tz>().is_ok() {
        return Ok(());
    }
    Err(AppError::Validation(format!(
        "Unknown IANA timezone '{timezone}' (expected e.g. 'Europe/Berlin' or 'auto')"
    )))
}

// =============================================================================
/// Validates a filesystem path string against control characters and `..` traversal.
///
/// # Errors
/// Returns [`AppError::Validation`] if `path` is empty, untrimmed, or contains `..`.
pub fn validate_safe_path(path: &str, label: &str) -> AppResult<()> {
    if path.is_empty() || path != path.trim() || path.starts_with('-') {
        return Err(AppError::Validation(format!(
            "{label} must not be empty, untrimmed, or start with '-'"
        )));
    }
    if path.chars().any(char::is_control) {
        return Err(AppError::Validation(format!(
            "{label} '{path}' contains control characters"
        )));
    }
    if Path::new(path)
        .components()
        .any(|comp| matches!(comp, Component::ParentDir))
    {
        return Err(AppError::Validation(format!(
            "{label} '{path}' must not contain '..' path traversal segments"
        )));
    }
    Ok(())
}
