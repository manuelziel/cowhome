//! HTTP API authentication (`RFC 6750`) and anti-spoofing client IP resolution (`OWASP`).
//!
//! Mirrors `02_linux/_project_template/src/core/security.py`: enforces fail-closed
//! constant-time API key verification (`Bearer`, `ApiKey`, `X-API-Key`) and resolves
//! client IP addresses across reverse proxies via right-to-left `X-Forwarded-For` traversal.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::core::constants::route_paths::{AUTH_SCHEME_APIKEY, AUTH_SCHEME_BEARER};
use crate::core::error::{AppError, AppResult};
use crate::core::validation::{constant_time_eq, is_api_key_secure, parse_ip_or_cidr};

// =============================================================================
/// Extracts a bearer or header API key from `Authorization` or `X-API-Key` headers.
#[must_use]
pub fn extract_request_api_key<'a>(
    authorization_header: Option<&'a str>,
    x_api_key_header: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(auth) = authorization_header.map(str::trim)
        && let Some((scheme, token)) = auth.split_once(char::is_whitespace)
        && (scheme.eq_ignore_ascii_case(AUTH_SCHEME_BEARER)
            || scheme.eq_ignore_ascii_case(AUTH_SCHEME_APIKEY))
    {
        let trimmed_tok = token.trim();
        if !trimmed_tok.is_empty() {
            return Some(trimmed_tok);
        }
    }
    x_api_key_header.map(str::trim).filter(|s| !s.is_empty())
}

// =============================================================================
/// Validates a client-supplied API key fail-closed using constant-time comparison.
///
/// # Errors
/// Returns [`AppError::Auth`] if the server key is unset/insecure or `provided` mismatches.
pub fn validate_api_key(provided: Option<&str>, expected: &str) -> AppResult<()> {
    if !is_api_key_secure(expected) {
        return Err(AppError::Auth(
            "Server API key is unconfigured or fails OWASP security policy".into(),
        ));
    }
    let Some(candidate) = provided.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(AppError::Auth("Missing API key".into()));
    };
    if !constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
        return Err(AppError::Auth("Invalid API key".into()));
    }
    Ok(())
}

// =============================================================================
/// Unwraps IPv4-mapped IPv6 addresses (`::ffff:192.0.2.1` -> `192.0.2.1`, `RFC 4291`).
#[must_use]
pub const fn unwrap_ipv4_mapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

// =============================================================================
/// Resolves the originating client IP across trusted reverse proxies (`OWASP` right-to-left).
///
/// Forwarding headers (`X-Forwarded-For`, `X-Real-IP`) are ignored completely unless
/// `remote_ip` is loopback (`127.0.0.0/8`, `::1`) or matches `trusted_proxies`.
#[must_use]
pub fn resolve_client_ip(
    remote_ip: IpAddr,
    x_forwarded_for: Option<&str>,
    x_real_ip: Option<&str>,
    trusted_proxies: &[String],
) -> IpAddr {
    let direct = unwrap_ipv4_mapped(remote_ip);
    if !is_trusted_proxy(direct, trusted_proxies) {
        return direct;
    }
    if let Some(xff) = x_forwarded_for
        && let Some(resolved) = walk_xff_right_to_left(xff, trusted_proxies)
    {
        return resolved;
    }
    if let Some(real_raw) = x_real_ip.map(str::trim)
        && let Ok(parsed) = real_raw.parse::<IpAddr>()
    {
        return unwrap_ipv4_mapped(parsed);
    }
    direct
}

// =============================================================================
/// Traverses `X-Forwarded-For` hops right-to-left, returning the first non-trusted IP.
fn walk_xff_right_to_left(xff: &str, trusted_proxies: &[String]) -> Option<IpAddr> {
    let mut rightmost_valid: Option<IpAddr> = None;
    for hop in xff.rsplit(',') {
        let Ok(raw_ip) = hop.trim().parse::<IpAddr>() else {
            continue;
        };
        let normalized = unwrap_ipv4_mapped(raw_ip);
        if rightmost_valid.is_none() {
            rightmost_valid = Some(normalized);
        }
        if !is_trusted_proxy(normalized, trusted_proxies) {
            return Some(normalized);
        }
    }
    rightmost_valid
}

// =============================================================================
/// Returns `true` if `ip` is loopback or matches any IP/CIDR in `trusted_proxies`.
#[must_use]
pub fn is_trusted_proxy(ip: IpAddr, trusted_proxies: &[String]) -> bool {
    let normalized = unwrap_ipv4_mapped(ip);
    if normalized.is_loopback() {
        return true;
    }
    for entry in trusted_proxies {
        if let Ok((net_ip, prefix)) = parse_ip_or_cidr(entry)
            && ip_in_cidr(normalized, unwrap_ipv4_mapped(net_ip), prefix)
        {
            return true;
        }
    }
    false
}

// =============================================================================
/// Tests whether `candidate` belongs to `(network, prefix_len)` via bitmask comparison.
#[must_use]
pub const fn ip_in_cidr(candidate: IpAddr, network: IpAddr, prefix_len: u8) -> bool {
    match (candidate, network) {
        (IpAddr::V4(cand), IpAddr::V4(net)) => ipv4_in_cidr(cand, net, prefix_len),
        (IpAddr::V6(cand), IpAddr::V6(net)) => ipv6_in_cidr(cand, net, prefix_len),
        _ => false,
    }
}

// =============================================================================
/// Tests IPv4 CIDR membership (`prefix_len` in `0..=32`).
const fn ipv4_in_cidr(cand: Ipv4Addr, net: Ipv4Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len >= 32 {
        return cand.to_bits() == net.to_bits();
    }
    let mask = u32::MAX << (32 - prefix_len);
    (cand.to_bits() & mask) == (net.to_bits() & mask)
}

// =============================================================================
/// Tests IPv6 CIDR membership (`prefix_len` in `0..=128`).
const fn ipv6_in_cidr(cand: Ipv6Addr, net: Ipv6Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len >= 128 {
        return cand.to_bits() == net.to_bits();
    }
    let mask = u128::MAX << (128 - prefix_len);
    (cand.to_bits() & mask) == (net.to_bits() & mask)
}
