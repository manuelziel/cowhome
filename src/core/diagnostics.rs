//! Core diagnostics primitives: process clock, range validation, JSON redaction,
//! POSIX `statvfs` disk-space probing, and `journalctl` / ring-buffer log collection.
//!
//! Shared across [`crate::services::diagnostics::DiagnosticsService`], CLI status views,
//! and SQLite storage diagnostics without upward service dependencies.

use std::ffi::CString;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::core::constants::app_meta::SYSLOG_IDENTIFIER;
use crate::core::constants::diagnostics_defaults::{
    DEFAULT_RANGE, SENSITIVE_KEY_TOKENS, SUPPORTED_RANGES,
};
use crate::core::constants::service_defaults::REDACTED_VALUE;
use crate::core::error::{AppError, AppResult};
use crate::core::logger::{LogRecord, Logger};
use crate::core::process::run_command_captured;
use crate::core::time_util::{format_epoch_utc_iso8601, now_utc_iso8601};
use crate::core::validation::sanitize_log_uri;

/// Monotonic process start instant initialized on first access.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

// =============================================================================
/// Optional query filter parameters matching `GET /api/v1/system/diagnostics`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticsQuery {
    /// Historical aggregation window (`"24h"`, `"7d"`, `"30d"`, `"90d"`, `"180d"`, `"365d"`, `"1y"`, or `"all"`).
    pub range: Option<String>,
    /// Maximum recent log entries to include (`1`..=`200`).
    pub limit: Option<usize>,
    /// Optional device identifier or substring filter for Block 4.
    pub device: Option<String>,
}

// =============================================================================
/// Initializes [`PROCESS_START`] early during `main()` startup.
pub fn init_process_clock() {
    PROCESS_START.get_or_init(Instant::now);
}

// =============================================================================
/// Returns elapsed process uptime in seconds rounded to `0.1 s`.
#[must_use]
pub fn process_uptime_seconds() -> f64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    (start.elapsed().as_secs_f64() * 10.0).round() / 10.0
}

// =============================================================================
/// Validates a requested diagnostics `range` token against [`SUPPORTED_RANGES`].
///
/// # Errors
/// Returns [`AppError::Validation`] if `range` is not in [`SUPPORTED_RANGES`].
pub fn validate_diagnostics_range(range: Option<&str>) -> AppResult<&'static str> {
    let candidate = range.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = candidate else {
        return Ok(DEFAULT_RANGE);
    };
    for &supported in SUPPORTED_RANGES {
        if raw.eq_ignore_ascii_case(supported) {
            return Ok(supported);
        }
    }
    Err(AppError::Validation(format!(
        "Unsupported diagnostics range '{raw}' (expected one of: {})",
        SUPPORTED_RANGES.join(", ")
    )))
}

// =============================================================================
/// Recursively redacts sensitive credential keys inside a `serde_json::Value` tree.
#[must_use]
pub fn redact_sensitive_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                if is_sensitive_json_key(k) && !v.is_object() {
                    out.insert(k.clone(), Value::String(REDACTED_VALUE.to_owned()));
                } else {
                    out.insert(k.clone(), redact_sensitive_json(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_sensitive_json).collect()),
        other => other.clone(),
    }
}

// =============================================================================
/// Checks whether a JSON key name represents a secret, password, hash, SID, or token.
fn is_sensitive_json_key(key: &str) -> bool {
    let lower = key.trim().to_ascii_lowercase();
    if SENSITIVE_KEY_TOKENS
        .iter()
        .any(|tok| lower == *tok || lower.contains(tok))
    {
        return true;
    }
    lower
        .split(['_', '-'])
        .any(|seg| matches!(seg, "pass" | "hash" | "sid"))
}

// =============================================================================
/// Disk space snapshot obtained from POSIX `statvfs`.
#[derive(Debug, Clone, Copy)]
pub struct DiskSpaceStats {
    /// Free bytes available to non-privileged users (`f_bavail * f_frsize`).
    pub free_bytes: u64,
    /// Percentage of total filesystem blocks currently in use (`0.0`..=`100.0`).
    pub used_percent: f64,
}

// =============================================================================
/// Queries filesystem usage on `path` via POSIX `libc::statvfs`.
#[must_use]
pub fn query_disk_space(path: &Path) -> Option<DiskSpaceStats> {
    let target = if path.exists() { path } else { Path::new(".") };
    let Ok(c_path) = CString::new(target.to_str()?) else {
        return None;
    };
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `vfs` is a valid writable stack struct.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 || vfs.f_blocks == 0 {
        return None;
    }
    let frsize = vfs.f_frsize as u64;
    let total = (vfs.f_blocks as u64).saturating_mul(frsize);
    let avail = (vfs.f_bavail as u64).saturating_mul(frsize);
    let used = total.saturating_sub((vfs.f_bfree as u64).saturating_mul(frsize));
    let pct = ((used as f64 / total as f64) * 1000.0).round() / 10.0;
    Some(DiskSpaceStats {
        free_bytes: avail,
        used_percent: pct,
    })
}

// =============================================================================
/// Reads recent `journalctl -o json` entries with automatic RAM ring-buffer fallback.
#[must_use]
pub fn collect_recent_logs(logger: &Logger, limit: usize, errors_only: bool) -> Vec<LogRecord> {
    let limit_str = limit.to_string();
    let mut args = vec![
        "-t",
        SYSLOG_IDENTIFIER,
        "-n",
        &limit_str,
        "-o",
        "json",
        "--no-pager",
    ];
    if errors_only {
        args.extend(["-p", "0..4"]);
    }
    if let Ok(out) = run_command_captured("journalctl", &args, Duration::from_secs(5)) {
        let parsed: Vec<LogRecord> = out
            .stdout
            .lines()
            .filter_map(parse_journal_json_line)
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    if errors_only {
        logger.recent_error_logs(limit)
    } else {
        logger.recent_logs(limit)
    }
}

// =============================================================================
/// Parses one `journalctl -o json` line into a sanitized [`LogRecord`].
fn parse_journal_json_line(line: &str) -> Option<LogRecord> {
    let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
        return None;
    };
    let usec: u64 = entry
        .get("__REALTIME_TIMESTAMP")
        .and_then(Value::as_str)
        .map_or(0, |s| s.parse::<u64>().unwrap_or(0));
    let timestamp = if usec > 0 {
        format_epoch_utc_iso8601(usec / 1_000_000)
    } else {
        now_utc_iso8601()
    };
    let priority = match entry.get("PRIORITY").and_then(Value::as_str).unwrap_or("6") {
        "0" | "1" | "2" => "CRITICAL",
        "3" => "ERROR",
        "4" => "WARNING",
        "7" => "DEBUG",
        _ => "INFO",
    };
    let msg = entry.get("MESSAGE").and_then(Value::as_str).unwrap_or("");
    Some(LogRecord {
        timestamp,
        priority: priority.to_owned(),
        message: sanitize_log_uri(msg),
    })
}
