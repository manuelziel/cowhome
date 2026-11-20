//! Thread-safe `ISO 8601` (`RFC 3339`), `IANA` timezone (`chrono-tz`), and LWW sync utilities.
//!
//! Combines Pure-Rust `chrono` & `chrono-tz` home-location wall-clock helpers with
//! Last-Write-Wins (LWW) client timestamp normalization, clock-skew clamping, and
//! direct-command TTL expiration checks.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, Offset, TimeZone, Timelike, Utc};
use chrono_tz::{OffsetName, Tz};
use serde_json::Value;

use crate::core::domain_constants::{
    analytics::efficiency_defaults::{MILLIS_PER_SECOND, SECONDS_PER_DAY},
    automation::{
        sync_defaults::{COMMAND_TTL_SECONDS, MILLISECOND_THRESHOLD, TTL_PROTECTED_ACTIONS},
        weather_defaults::{TIMEZONE, TIMEZONE_AUTO},
    },
};
use crate::core::error::{AppError, AppResult};

// =============================================================================
/// Returns the current UNIX epoch timestamp in fractional seconds (`f64`).
#[must_use]
pub fn now_epoch_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// =============================================================================
/// Strips ASCII spaces from a FritzBox AIN or group identifier (`"15282 0905546"` -> `"152820905546"`).
#[must_use]
pub fn normalize_ain(raw: &str) -> String {
    raw.chars().filter(|ch| !ch.is_ascii_whitespace()).collect()
}

// =============================================================================
/// Resolves an optional `IANA` timezone string (`"Europe/Berlin"` or `"auto"`) to [`Tz`].
///
/// Falls back deterministically to `Europe/Berlin` when `tz_name` is `None`, empty,
/// `"auto"`, or unrecognized.
#[must_use]
pub fn resolve_home_tz(tz_name: Option<&str>) -> Tz {
    let Some(raw) = tz_name.map(str::trim).filter(|s| !s.is_empty()) else {
        return chrono_tz::Europe::Berlin;
    };
    if raw.eq_ignore_ascii_case(TIMEZONE_AUTO) {
        return TIMEZONE.parse::<Tz>().unwrap_or(chrono_tz::Europe::Berlin);
    }
    raw.parse::<Tz>().unwrap_or(chrono_tz::Europe::Berlin)
}

// =============================================================================
/// Resolves the UNIX timestamp (`f64` seconds) of `00:00:00` local time today in `tz_name`.
#[must_use]
pub fn resolve_local_midnight_ts(now_ts: f64, tz_name: Option<&str>) -> f64 {
    let tz = resolve_home_tz(tz_name);
    let secs = now_ts.floor() as i64;
    let Some(now_local) = tz.timestamp_opt(secs, 0).single() else {
        return now_ts - SECONDS_PER_DAY;
    };
    let midnight = now_local
        .with_hour(0)
        .and_then(|dt| dt.with_minute(0))
        .and_then(|dt| dt.with_second(0))
        .and_then(|dt| dt.with_nanosecond(0));
    let midnight_ts = midnight.map_or(now_ts - SECONDS_PER_DAY, |dt| dt.timestamp() as f64);
    if midnight_ts < now_ts {
        midnight_ts
    } else {
        now_ts - SECONDS_PER_DAY
    }
}

// =============================================================================
/// Normalizes an optional client timestamp (JSON body or `X-Client-Timestamp` header).
///
/// Automatically converts millisecond epochs (`> 100_000_000_000`) to seconds and clamps
/// future client clocks to `min(client_ts, server_now)`.
///
/// # Errors
/// Returns [`AppError::Validation`] if an explicit timestamp is boolean, non-numeric, or `<= 0`.
pub fn normalize_client_timestamp(
    body_value: Option<&Value>,
    header_value: Option<&str>,
    server_now: Option<f64>,
) -> AppResult<(f64, bool)> {
    let now = server_now.unwrap_or_else(now_epoch_f64);
    if let Some(val) = body_value.filter(|v| !v.is_null()) {
        let parsed = parse_json_timestamp(val)?;
        return finalize_timestamp(parsed, now);
    }
    if let Some(hdr) = header_value.map(str::trim).filter(|s| !s.is_empty()) {
        let parsed: f64 = hdr.parse().map_err(|_| {
            AppError::Validation(format!(
                "Invalid X-Client-Timestamp '{hdr}': must be numeric"
            ))
        })?;
        return finalize_timestamp(parsed, now);
    }
    Ok((now, false))
}

// =============================================================================
/// Parses a JSON number or numeric string as a raw timestamp, rejecting booleans/objects.
///
/// # Errors
/// Returns [`AppError::Validation`] if `val` is a boolean or cannot be parsed as `f64`.
fn parse_json_timestamp(val: &Value) -> AppResult<f64> {
    match val {
        Value::Bool(_) => Err(AppError::Validation(
            "Invalid client_timestamp: boolean is not allowed".into(),
        )),
        Value::Number(num) => num.as_f64().ok_or_else(|| {
            AppError::Validation("Invalid client_timestamp: number out of range".into())
        }),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(0.0);
            }
            trimmed.parse::<f64>().map_err(|_| {
                AppError::Validation(format!(
                    "Invalid client_timestamp '{trimmed}': must be numeric"
                ))
            })
        }
        other => Err(AppError::Validation(format!(
            "Invalid client_timestamp '{other}': must be numeric"
        ))),
    }
}

// =============================================================================
/// Validates positivity, converts milliseconds to seconds, and clamps against `now`.
///
/// # Errors
/// Returns [`AppError::Validation`] if `raw_ts` is non-finite or `<= 0.0`.
fn finalize_timestamp(raw_ts: f64, now: f64) -> AppResult<(f64, bool)> {
    if !raw_ts.is_finite() || raw_ts <= 0.0 {
        return Err(AppError::Validation(
            "Invalid client_timestamp: must be a positive timestamp".into(),
        ));
    }
    let seconds = if raw_ts > MILLISECOND_THRESHOLD {
        raw_ts / MILLIS_PER_SECOND
    } else {
        raw_ts
    };
    Ok((seconds.min(now), true))
}

// =============================================================================
/// Checks whether an explicit direct hardware/router command has exceeded [`COMMAND_TTL_SECONDS`].
#[must_use]
pub fn is_command_expired(
    action: &str,
    effective_ts: f64,
    is_explicit: bool,
    server_now: Option<f64>,
) -> bool {
    if !is_explicit || !TTL_PROTECTED_ACTIONS.contains(&action) {
        return false;
    }
    let now = server_now.unwrap_or_else(now_epoch_f64);
    (now - effective_ts) > COMMAND_TTL_SECONDS
}

// =============================================================================
/// Builds a canonical LWW tracking key (`action:<normalized_ain>`) for device commands.
#[must_use]
pub fn build_action_lww_key(ain: &str) -> String {
    format!("action:{}", normalize_ain(ain))
}

// =============================================================================
/// Builds a canonical LWW tracking key (`name:<normalized_ain>`) for custom device names.
#[must_use]
pub fn build_name_lww_key(ain: &str) -> String {
    format!("name:{}", normalize_ain(ain))
}

// =============================================================================
/// Builds a canonical LWW tracking key (`group:<group_id>`) for software groups.
#[must_use]
pub fn build_group_lww_key(group_id: i64) -> String {
    format!("group:{group_id}")
}

// =============================================================================
/// Builds a canonical LWW tracking key (`automation:<scope_type>:<scope_id>:<feature>`).
#[must_use]
pub fn build_automation_lww_key(scope_type: &str, scope_id: &str, feature_name: &str) -> String {
    format!(
        "automation:{scope_type}:{}:{feature_name}",
        normalize_ain(scope_id)
    )
}

// =============================================================================
/// Builds a canonical LWW tracking key (`setting:<setting_key>`) for `system_settings`.
#[must_use]
pub fn build_setting_lww_key(setting_key: &str) -> String {
    format!("setting:{}", setting_key.trim())
}

// =============================================================================
/// Returns the current wall-clock time as an `ISO 8601` UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
#[must_use]
pub fn now_utc_iso8601() -> String {
    format_epoch_utc_iso8601(current_epoch_secs())
}

// =============================================================================
/// Returns the current wall-clock time as an `ISO 8601` local string with UTC offset.
#[must_use]
pub fn now_local_iso8601() -> String {
    format_epoch_local_iso8601(current_epoch_secs())
}

// =============================================================================
/// Formats a UNIX epoch second count as `"YYYY-MM-DDTHH:MM:SSZ"`.
#[must_use]
pub fn format_epoch_utc_iso8601(epoch_secs: u64) -> String {
    let secs: i64 = epoch_secs.try_into().unwrap_or(0);
    let Some(dt) = DateTime::from_timestamp(secs, 0) else {
        return String::from("1970-01-01T00:00:00Z");
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

// =============================================================================
/// Formats a UNIX epoch second count as `"YYYY-MM-DDTHH:MM:SS±HH:MM"` in OS local time.
#[must_use]
pub fn format_epoch_local_iso8601(epoch_secs: u64) -> String {
    let secs: i64 = epoch_secs.try_into().unwrap_or(0);
    let Some(utc_dt) = DateTime::from_timestamp(secs, 0) else {
        return format_epoch_utc_iso8601(epoch_secs);
    };
    let local_dt = Local.from_utc_datetime(&utc_dt.naive_utc());
    let off_secs = local_dt.offset().fix().local_minus_utc();
    let sign = if off_secs >= 0 { '+' } else { '-' };
    let abs_mins = off_secs.unsigned_abs() / 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        local_dt.year(),
        local_dt.month(),
        local_dt.day(),
        local_dt.hour(),
        local_dt.minute(),
        local_dt.second(),
        sign,
        abs_mins / 60,
        abs_mins % 60
    )
}

// =============================================================================
/// Formats a UNIX epoch second count as human-readable local `"YYYY-MM-DD HH:MM:SS"`.
#[must_use]
pub fn format_epoch_local_human(epoch_secs: u64) -> String {
    let secs: i64 = epoch_secs.try_into().unwrap_or(0);
    let Some(utc_dt) = DateTime::from_timestamp(secs, 0) else {
        return String::from("unknown");
    };
    let local_dt = Local.from_utc_datetime(&utc_dt.naive_utc());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local_dt.year(),
        local_dt.month(),
        local_dt.day(),
        local_dt.hour(),
        local_dt.minute(),
        local_dt.second()
    )
}

// =============================================================================
/// Reads the active OS timezone abbreviation (e.g., `"CET"`, `"CEST"`, `"UTC"`).
#[must_use]
pub fn current_os_timezone_abbr() -> String {
    if let Some(tz) = detect_os_iana_tz() {
        if let Some(abbr) = Utc::now().with_timezone(&tz).offset().abbreviation() {
            if !abbr.is_empty() {
                return abbr.to_owned();
            }
        }
    }
    let off_secs = Local::now().offset().fix().local_minus_utc();
    if off_secs == 0 {
        return String::from("UTC");
    }
    let sign = if off_secs >= 0 { '+' } else { '-' };
    let abs_mins = off_secs.unsigned_abs() / 60;
    format!("UTC{}{:02}:{:02}", sign, abs_mins / 60, abs_mins % 60)
}

// =============================================================================
/// Detects the host IANA timezone from `TZ` or `/etc/localtime` symlink target.
fn detect_os_iana_tz() -> Option<Tz> {
    if let Ok(env_tz) = std::env::var("TZ") {
        if let Ok(tz) = env_tz.trim().trim_start_matches(':').parse::<Tz>() {
            return Some(tz);
        }
    }
    let Ok(link) = std::fs::read_link("/etc/localtime") else {
        return None;
    };
    let path_str = link.to_string_lossy();
    let (_, iana_part) = path_str.split_once("zoneinfo/")?;
    iana_part.parse::<Tz>().ok()
}

// =============================================================================
/// Returns the current UNIX timestamp in whole seconds.
fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
