//! FritzBox authentication primitives: AHA Login-SID (`v2` PBKDF2 / `v1` MD5) & TR-064 Digest.
//!
//! Implements AVM's `/login_sid.lua?version=2` (`PBKDF2-HMAC-SHA256`) session handshake
//! with automatic fallback to `v1` (`MD5` UTF-16LE), plus stateful `RFC 2617` / `RFC 7616`
//! HTTP Digest authentication for TR-064 SOAP endpoints on port `49000`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use md5::{Digest as _, Md5};
use pbkdf2::pbkdf2_hmac;
use reqwest::Client;
use roxmltree::Document;
use sha2::Sha256;

use crate::core::domain_constants::hardware::{
    fritz_api::{AHA_CHALLENGE_V2_PREFIX, AHA_LOGIN_PATH, AHA_LOGIN_V2_PATH},
    tr064_defaults::{INVALID_SID, MAX_PBKDF2_ITERATIONS, TR064_PORT},
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::now_epoch_f64;
use crate::core::validation::sanitize_log_uri;

/// Monotonic counter mixed into HTTP Digest `cnonce` generation.
static CNONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

// =============================================================================
/// Parsed `<SessionInfo>` XML fields returned by FritzBox `/login_sid.lua`.
#[derive(Debug, Default)]
struct SessionInfoXml {
    /// 16-hex-character session ID (`0000000000000000` when unauthenticated).
    sid: String,
    /// Login challenge (`2$iter1$salt1$iter2$salt2` for v2 or 8-hex string for v1).
    challenge: String,
    /// Active brute-force block duration in seconds (`> 0` rejects login immediately).
    block_time: u32,
}

// =============================================================================
/// Cached `WWW-Authenticate: Digest` challenge state for preemptive TR-064 requests.
#[derive(Debug, Clone)]
struct DigestChallenge {
    /// Protection realm (`"HTTPS Access"`, `"fritz.box"`, etc.).
    realm: String,
    /// Server-issued nonce token.
    nonce: String,
    /// Optional server opaque token echoed verbatim.
    opaque: Option<String>,
    /// Selected quality-of-protection (`Some("auth")` or `None`).
    qop: Option<String>,
    /// Selected hash algorithm (`"MD5"`, `"MD5-sess"`, `"SHA-256"`, `"SHA-256-sess"`).
    algorithm: String,
    /// Monotonic per-nonce request counter (`nc`).
    nc: u32,
}

// =============================================================================
/// Stateful `RFC 2617` / `RFC 7616` HTTP Digest authenticator for TR-064 SOAP requests.
#[derive(Debug, Default)]
pub struct DigestAuthenticator {
    /// Cached challenge avoiding redundant `401` round-trips on steady-state polls.
    challenge: Mutex<Option<DigestChallenge>>,
}

impl DigestAuthenticator {
    /// Creates an empty [`DigestAuthenticator`] with no cached server nonce.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears any cached challenge (invoked when credentials or host change on `SIGHUP`).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.challenge.lock() {
            *guard = None;
        }
    }

    /// Builds a preemptive `Authorization: Digest ...` header if a challenge is cached.
    #[must_use]
    pub fn preemptive_header(
        &self,
        user: &str,
        password: &str,
        method: &str,
        uri_path: &str,
    ) -> Option<String> {
        if user.is_empty() && password.is_empty() {
            return None;
        }
        let mut guard = self.challenge.lock().ok()?;
        let ch = guard.as_mut()?;
        ch.nc = ch.nc.wrapping_add(1).max(1);
        Some(format_digest_authorization(
            ch, user, password, method, uri_path,
        ))
    }

    /// Parses a `401` `WWW-Authenticate: Digest ...` header, caches it, and returns `Authorization`.
    ///
    /// # Errors
    /// Returns [`AppError::Auth`] if the header is not a valid HTTP Digest challenge.
    pub fn handle_unauthorized(
        &self,
        www_authenticate: &str,
        user: &str,
        password: &str,
        method: &str,
        uri_path: &str,
    ) -> AppResult<String> {
        let mut parsed = parse_digest_challenge(www_authenticate)?;
        parsed.nc = 1;
        let header_val = format_digest_authorization(&parsed, user, password, method, uri_path);
        if let Ok(mut guard) = self.challenge.lock() {
            *guard = Some(parsed);
        }
        Ok(header_val)
    }
}

// =============================================================================
/// Normalizes a FritzBox host or URL into an AHA HTTP(S) base URL without trailing slash.
#[must_use]
pub fn build_aha_base_url(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("http://[{trimmed}]")
    } else {
        format!("http://{trimmed}")
    }
}

// =============================================================================
/// Normalizes a FritzBox host into a TR-064 HTTP base URL on port `49000`.
#[must_use]
pub fn build_tr064_base_url(host: &str) -> String {
    let trimmed = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if let Some((addr, port)) = trimmed.rsplit_once(':') {
        if port.chars().all(|c| c.is_ascii_digit()) && !addr.contains(':') {
            return format!("http://{addr}:{port}");
        }
        if addr.starts_with('[') && addr.ends_with(']') && port.chars().all(|c| c.is_ascii_digit())
        {
            return format!("http://{addr}:{port}");
        }
    }
    if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("http://[{trimmed}]:{TR064_PORT}")
    } else {
        format!("http://{trimmed}:{TR064_PORT}")
    }
}

// =============================================================================
/// Percent-encodes an arbitrary string per `RFC 3986` unreserved character rules.
#[must_use]
pub fn percent_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// =============================================================================
/// Returns `true` when `sid` is a non-empty, non-zero FritzBox Session ID.
#[must_use]
pub fn is_valid_sid(sid: &str) -> bool {
    let trimmed = sid.trim();
    trimmed.len() == INVALID_SID.len()
        && trimmed != INVALID_SID
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
}

// =============================================================================
/// Performs a FritzBox AHA login (`v2` PBKDF2-HMAC-SHA256 with `v1` MD5 fallback).
///
/// # Errors
/// Returns [`AppError::Auth`] on bad credentials/block-time or [`AppError::Service`] on I/O error.
pub async fn login_aha_sid(
    client: &Client,
    base_url: &str,
    user: &str,
    password: &str,
) -> AppResult<String> {
    let init_url = format!("{base_url}{AHA_LOGIN_V2_PATH}");
    let init_xml = fetch_text(client.get(&init_url), "AHA login challenge").await?;
    let info = parse_session_info_xml(&init_xml)?;
    if info.block_time > 0 {
        return Err(AppError::Auth(format!(
            "FritzBox login temporarily blocked for {}s",
            info.block_time
        )));
    }
    if is_valid_sid(&info.sid) {
        return Ok(info.sid);
    }
    if info.challenge.starts_with(AHA_CHALLENGE_V2_PREFIX) {
        let resp_token = solve_v2_pbkdf2_challenge(&info.challenge, password)?;
        let body = format!(
            "username={}&response={}",
            percent_encode_component(user),
            percent_encode_component(&resp_token)
        );
        let req = client
            .post(&init_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        let auth_xml = fetch_text(req, "AHA v2 login response").await?;
        extract_authenticated_sid(&auth_xml)
    } else if !info.challenge.is_empty() {
        let resp_token = solve_v1_md5_challenge(&info.challenge, password);
        let auth_url = format!(
            "{base_url}{AHA_LOGIN_PATH}?username={}&response={}",
            percent_encode_component(user),
            percent_encode_component(&resp_token)
        );
        let auth_xml = fetch_text(client.get(&auth_url), "AHA v1 login response").await?;
        extract_authenticated_sid(&auth_xml)
    } else {
        Err(AppError::Auth(
            "FritzBox returned empty login challenge".into(),
        ))
    }
}

// =============================================================================
/// Executes an outbound HTTP request and reads its UTF-8 response body.
///
/// # Errors
/// Returns [`AppError::Auth`] on `401`/`403` or [`AppError::Service`] on transport/HTTP failure.
async fn fetch_text(builder: reqwest::RequestBuilder, context: &str) -> AppResult<String> {
    let resp = builder.send().await.map_err(|e| {
        let safe = sanitize_log_uri(&e.to_string());
        AppError::Service(format!("{context} network error: {safe}"))
    })?;
    let code = resp.status().as_u16();
    if code == 401 || code == 403 {
        return Err(AppError::Auth(format!("{context} rejected (HTTP {code})")));
    }
    if !resp.status().is_success() {
        return Err(AppError::Service(format!(
            "{context} failed with HTTP {code}"
        )));
    }
    resp.text().await.map_err(|e| {
        let safe = sanitize_log_uri(&e.to_string());
        AppError::Service(format!("{context} body decode error: {safe}"))
    })
}

// =============================================================================
/// Parses `<SessionInfo>` XML returned by `/login_sid.lua`.
///
/// # Errors
/// Returns [`AppError::Service`] if the response is not valid XML.
fn parse_session_info_xml(xml: &str) -> AppResult<SessionInfoXml> {
    let doc = Document::parse(xml)
        .map_err(|e| AppError::Service(format!("Invalid SessionInfo XML: {e}")))?;
    let root = doc.root_element();
    let find = |tag: &str| {
        root.children()
            .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(tag))
            .and_then(|n| n.text())
            .map(str::trim)
            .unwrap_or("")
    };
    Ok(SessionInfoXml {
        sid: find("SID").to_owned(),
        challenge: find("Challenge").to_owned(),
        block_time: find("BlockTime").parse::<u32>().unwrap_or(0),
    })
}

// =============================================================================
/// Validates that a post-login `<SessionInfo>` contains an active non-zero SID.
///
/// # Errors
/// Returns [`AppError::Auth`] when credentials were rejected (`SID == 0000000000000000`).
fn extract_authenticated_sid(xml: &str) -> AppResult<String> {
    let info = parse_session_info_xml(xml)?;
    if is_valid_sid(&info.sid) {
        return Ok(info.sid);
    }
    Err(AppError::Auth(format!(
        "Invalid FritzBox credentials (block_time: {}s)",
        info.block_time
    )))
}

// =============================================================================
/// Computes AVM Login-SID v2 `2$iter1$salt1$iter2$salt2` (`PBKDF2-HMAC-SHA256`).
///
/// # Errors
/// Returns [`AppError::Auth`] if challenge parameters are malformed or exceed iteration limits.
pub(crate) fn solve_v2_pbkdf2_challenge(challenge: &str, password: &str) -> AppResult<String> {
    let parts: Vec<&str> = challenge.split('$').collect();
    if parts.len() != 5 || parts[0] != "2" {
        return Err(AppError::Auth("Malformed FritzBox v2 challenge".into()));
    }
    let iter1 = parse_bounded_iterations(parts[1])?;
    let salt1 = hex::decode(parts[2])
        .map_err(|_| AppError::Auth("Invalid hex salt1 in FritzBox v2 challenge".into()))?;
    let iter2 = parse_bounded_iterations(parts[3])?;
    let salt2 = hex::decode(parts[4])
        .map_err(|_| AppError::Auth("Invalid hex salt2 in FritzBox v2 challenge".into()))?;

    let mut hash1 = [0_u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt1, iter1, &mut hash1);
    let mut hash2 = [0_u8; 32];
    pbkdf2_hmac::<Sha256>(&hash1, &salt2, iter2, &mut hash2);
    Ok(format!("{}${}", parts[4], hex::encode(hash2)))
}

// =============================================================================
/// Parses and bounds a PBKDF2 iteration count (`1..=MAX_PBKDF2_ITERATIONS`).
///
/// # Errors
/// Returns [`AppError::Auth`] if the iteration count is zero, non-numeric, or excessive.
fn parse_bounded_iterations(raw: &str) -> AppResult<u32> {
    let val = raw
        .parse::<u32>()
        .map_err(|_| AppError::Auth("Invalid PBKDF2 iteration count in challenge".into()))?;
    if val == 0 || val > MAX_PBKDF2_ITERATIONS {
        return Err(AppError::Auth(format!(
            "PBKDF2 iteration count {val} outside safe bounds"
        )));
    }
    Ok(val)
}

// =============================================================================
/// Computes legacy AVM Login-SID v1 `MD5(UTF-16LE("{challenge}-{password}"))`.
#[must_use]
pub(crate) fn solve_v1_md5_challenge(challenge: &str, password: &str) -> String {
    let sanitized: String = password
        .chars()
        .map(|ch| if (ch as u32) > 255 { '.' } else { ch })
        .collect();
    let combined = format!("{challenge}-{sanitized}");
    let mut utf16le = Vec::with_capacity(combined.len() * 2);
    for code_unit in combined.encode_utf16() {
        utf16le.extend_from_slice(&code_unit.to_le_bytes());
    }
    let digest = Md5::digest(&utf16le);
    format!("{challenge}-{}", hex::encode(digest))
}

// =============================================================================
/// Parses a `WWW-Authenticate: Digest ...` response header into [`DigestChallenge`].
///
/// # Errors
/// Returns [`AppError::Auth`] if `Digest` scheme, `realm`, or `nonce` is missing.
fn parse_digest_challenge(header: &str) -> AppResult<DigestChallenge> {
    let rest = header
        .trim()
        .strip_prefix("Digest ")
        .or_else(|| header.trim().strip_prefix("digest "))
        .ok_or_else(|| AppError::Auth("Expected HTTP Digest challenge from TR-064".into()))?;
    let params = parse_digest_kv_pairs(rest);
    let realm = params
        .get("realm")
        .cloned()
        .ok_or_else(|| AppError::Auth("Missing 'realm' in TR-064 Digest challenge".into()))?;
    let nonce = params
        .get("nonce")
        .cloned()
        .ok_or_else(|| AppError::Auth("Missing 'nonce' in TR-064 Digest challenge".into()))?;
    let qop = params.get("qop").and_then(|q| {
        q.split(',')
            .map(str::trim)
            .find(|&t| t.eq_ignore_ascii_case("auth"))
            .map(|_| String::from("auth"))
    });
    let algorithm = params
        .get("algorithm")
        .cloned()
        .unwrap_or_else(|| String::from("MD5"));
    Ok(DigestChallenge {
        realm,
        nonce,
        opaque: params.get("opaque").cloned(),
        qop,
        algorithm,
        nc: 0,
    })
}

// =============================================================================
/// Splits comma-separated `key="value"` or `key=token` pairs in a Digest challenge.
fn parse_digest_kv_pairs(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in input.chars().chain(std::iter::once(',')) {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == ',' && !in_quotes {
            if let Some((k, v)) = cur.trim().split_once('=') {
                map.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    map
}

// =============================================================================
/// Formats a complete `Authorization: Digest ...` request header (`RFC 2617` / `RFC 7616`).
fn format_digest_authorization(
    ch: &DigestChallenge,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> String {
    let cnonce = generate_cnonce();
    let nc_hex = format!("{:08x}", ch.nc);
    let algo_upper = ch.algorithm.to_ascii_uppercase();
    let use_sha256 = algo_upper.starts_with("SHA-256");
    let hash_fn = |data: &str| digest_hex(data.as_bytes(), use_sha256);

    let base_ha1 = hash_fn(&format!("{user}:{}:{password}", ch.realm));
    let ha1 = if algo_upper.ends_with("-SESS") {
        hash_fn(&format!("{base_ha1}:{}:{cnonce}", ch.nonce))
    } else {
        base_ha1
    };
    let ha2 = hash_fn(&format!("{method}:{uri}"));
    let response = if ch.qop.as_deref() == Some("auth") {
        hash_fn(&format!("{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", ch.nonce))
    } else {
        hash_fn(&format!("{ha1}:{}:{ha2}", ch.nonce))
    };

    let mut hdr = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\", algorithm={}",
        escape_quoted(user),
        escape_quoted(&ch.realm),
        escape_quoted(&ch.nonce),
        ch.algorithm
    );
    if ch.qop.as_deref() == Some("auth") {
        hdr.push_str(&format!(", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\""));
    }
    if let Some(ref opaque) = ch.opaque {
        hdr.push_str(&format!(", opaque=\"{}\"", escape_quoted(opaque)));
    }
    hdr
}

// =============================================================================
/// Hashes `bytes` with `SHA-256` or `MD5` and returns lowercase hexadecimal.
fn digest_hex(bytes: &[u8], use_sha256: bool) -> String {
    if use_sha256 {
        hex::encode(Sha256::digest(bytes))
    } else {
        hex::encode(Md5::digest(bytes))
    }
}

// =============================================================================
/// Generates a unique 16-hex-char client nonce (`cnonce`) without external RNG crates.
fn generate_cnonce() -> String {
    let seq = CNONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{}:{seq}:{}", now_epoch_f64().to_bits(), std::process::id());
    let digest = Sha256::digest(seed.as_bytes());
    hex::encode(&digest[..8])
}

// =============================================================================
/// Escapes `\` and `"` inside quoted HTTP Digest header values.
fn escape_quoted(val: &str) -> String {
    val.replace('\\', "\\\\").replace('"', "\\\"")
}
