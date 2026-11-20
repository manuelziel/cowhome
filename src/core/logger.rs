//! Native `systemd-journald` socket logging, in-memory ring buffer, and `NO_COLOR` checks.
//!
//! Emits structured datagrams directly to `/run/systemd/journal/socket` without linking
//! `libsystemd`, redacts configured secrets automatically, and maintains a bounded
//! `200`-entry ring buffer for unprivileged `Universal Diagnostics Schema v1.0` queries.

use std::collections::VecDeque;
use std::fmt;
use std::io::IsTerminal;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::core::constants::app_meta::SYSLOG_IDENTIFIER;
use crate::core::constants::diagnostics_defaults::RING_BUFFER_CAPACITY;
use crate::core::constants::fhs_paths::JOURNALD_SOCKET_PATH;
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::now_utc_iso8601;
use crate::core::validation::{redact_text, sanitize_log_uri};

// =============================================================================
/// Semantic application log severity ordered from most verbose to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Unfiltered threshold matching `NOTSET`.
    Notset,
    /// Fine-grained diagnostic details for troubleshooting.
    Debug,
    /// Normal lifecycle and operational milestones.
    Info,
    /// Recoverable anomalies or skipped operations requiring attention.
    Warning,
    /// Failed operations affecting a request, cycle, or subcommand.
    Error,
    /// Severe system-level failures preventing application execution.
    Critical,
}

impl LogLevel {
    /// Parses a case-insensitive log level string strictly.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `raw` is not one of the supported log levels.
    pub fn parse_strict(raw: &str) -> AppResult<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "NOTSET" => Ok(Self::Notset),
            "DEBUG" => Ok(Self::Debug),
            "INFO" => Ok(Self::Info),
            "WARNING" | "WARN" => Ok(Self::Warning),
            "ERROR" => Ok(Self::Error),
            "CRITICAL" => Ok(Self::Critical),
            other => Err(AppError::Validation(format!(
                "Invalid debug.level '{other}' (expected DEBUG, INFO, WARNING, ERROR, CRITICAL, or NOTSET)"
            ))),
        }
    }

    /// Returns the canonical uppercase string representation for `config.toml` and JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notset => "NOTSET",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    /// Returns the RFC 5424 / `systemd-journald` numeric syslog priority (`2`..=`7`).
    #[must_use]
    pub const fn syslog_priority(self) -> u8 {
        match self {
            Self::Notset | Self::Debug => 7,
            Self::Info => 6,
            Self::Warning => 4,
            Self::Error => 3,
            Self::Critical => 2,
        }
    }
}

impl fmt::Display for LogLevel {
    /// Formats the log level as its uppercase identifier (`DEBUG`..`CRITICAL`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
/// Sanitized log record stored in the bounded in-memory diagnostics ring buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogRecord {
    /// `ISO 8601` UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
    pub timestamp: String,
    /// Canonical severity label (`DEBUG`..`CRITICAL`).
    pub priority: String,
    /// URI- and secret-sanitized log message.
    pub message: String,
}

// =============================================================================
/// Mutable runtime settings shared across cloned [`Logger`] handles.
#[derive(Debug)]
struct LoggerInner {
    /// Active minimum severity threshold.
    min_level: LogLevel,
    /// Pre-evaluated ANSI terminal color support flag.
    colors_enabled: bool,
    /// Configured secrets scrubbed from every log message (`api_key`, `fritzbox.password`).
    redacted_secrets: Vec<String>,
    /// Bounded FIFO ring buffer holding recent sanitized log records.
    ring_buffer: VecDeque<LogRecord>,
}

// =============================================================================
/// Evaluates `NO_COLOR`, `TERM=dumb`, TTY attachment, and configuration preference.
#[must_use]
pub fn should_use_colors(config_colors_enabled: bool, stream_is_tty: bool) -> bool {
    if !config_colors_enabled || !stream_is_tty {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.eq_ignore_ascii_case("dumb") {
            return false;
        }
    }
    true
}

// =============================================================================
/// Dependency-injected logger forwarding sanitized events to `journald` and a ring buffer.
#[derive(Debug, Clone)]
pub struct Logger {
    /// Shared mutex-guarded state across service and CLI components.
    inner: Arc<Mutex<LoggerInner>>,
}

impl Logger {
    /// Constructs a logger configured for the current `stderr` terminal environment.
    #[must_use]
    pub fn new(min_level: LogLevel, config_colors_enabled: bool) -> Self {
        let stderr_is_tty = std::io::stderr().is_terminal();
        let inner = LoggerInner {
            min_level,
            colors_enabled: should_use_colors(config_colors_enabled, stderr_is_tty),
            redacted_secrets: Vec::new(),
            ring_buffer: VecDeque::with_capacity(RING_BUFFER_CAPACITY),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Indicates whether ANSI escape sequences may be emitted on `stderr`.
    #[must_use]
    pub fn colors_enabled(&self) -> bool {
        self.inner.lock().is_ok_and(|guard| guard.colors_enabled)
    }

    /// Returns the active minimum severity threshold.
    #[must_use]
    pub fn min_level(&self) -> LogLevel {
        self.inner
            .lock()
            .map_or(LogLevel::Info, |guard| guard.min_level)
    }

    /// Updates `min_level`, `console_colors`, and redacted `secrets` on live `SIGHUP` reload.
    pub fn reconfigure(&self, min_level: LogLevel, config_colors_enabled: bool, secrets: &[&str]) {
        let stderr_is_tty = std::io::stderr().is_terminal();
        if let Ok(mut guard) = self.inner.lock() {
            guard.min_level = min_level;
            guard.colors_enabled = should_use_colors(config_colors_enabled, stderr_is_tty);
            guard.redacted_secrets = secrets
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }

    /// Records a `DEBUG` entry when enabled.
    pub fn debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    /// Records an `INFO` entry when enabled.
    pub fn info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    /// Records a `WARNING` entry when enabled.
    pub fn warning(&self, message: &str) {
        self.log(LogLevel::Warning, message);
    }

    /// Records an `ERROR` entry when enabled.
    pub fn error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }

    /// Records a `CRITICAL` entry.
    pub fn critical(&self, message: &str) {
        self.log(LogLevel::Critical, message);
    }

    /// Sanitizes `message`, appends to the ring buffer, and sends a `journald` datagram.
    pub fn log(&self, level: LogLevel, message: &str) {
        let sanitized = {
            let Ok(mut guard) = self.inner.lock() else {
                return;
            };
            if level < guard.min_level {
                return;
            }
            let mut clean = sanitize_log_uri(message);
            for secret in &guard.redacted_secrets {
                clean = redact_text(&clean, Some(secret.as_str()));
            }
            push_ring_record(&mut guard.ring_buffer, level, &clean);
            clean
        };
        send_journald_datagram(level, &sanitized);
    }

    /// Returns up to `limit` most recent sanitized log records from the in-memory buffer.
    #[must_use]
    pub fn recent_logs(&self, limit: usize) -> Vec<LogRecord> {
        let Ok(guard) = self.inner.lock() else {
            return Vec::new();
        };
        let skip = guard.ring_buffer.len().saturating_sub(limit);
        guard.ring_buffer.iter().skip(skip).cloned().collect()
    }

    /// Returns up to `limit` most recent `WARNING`/`ERROR`/`CRITICAL` records from RAM.
    #[must_use]
    pub fn recent_error_logs(&self, limit: usize) -> Vec<LogRecord> {
        let Ok(guard) = self.inner.lock() else {
            return Vec::new();
        };
        let filtered: Vec<LogRecord> = guard
            .ring_buffer
            .iter()
            .filter(|rec| matches!(rec.priority.as_str(), "WARNING" | "ERROR" | "CRITICAL"))
            .cloned()
            .collect();
        let skip = filtered.len().saturating_sub(limit);
        filtered.into_iter().skip(skip).collect()
    }
}

// =============================================================================
/// Pushes a sanitized [`LogRecord`] into `ring`, evicting the oldest item when full.
fn push_ring_record(ring: &mut VecDeque<LogRecord>, level: LogLevel, clean_msg: &str) {
    if ring.len() >= RING_BUFFER_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(LogRecord {
        timestamp: now_utc_iso8601(),
        priority: level.as_str().to_owned(),
        message: clean_msg.to_owned(),
    });
}

// =============================================================================
/// Appends a key-value field in native `systemd-journald` datagram wire format.
fn append_journal_field(buffer: &mut Vec<u8>, key: &str, value: &str) {
    buffer.extend_from_slice(key.as_bytes());
    if value.contains('\n') {
        buffer.push(b'\n');
        buffer.extend_from_slice(&(value.len() as u64).to_le_bytes());
        buffer.extend_from_slice(value.as_bytes());
        buffer.push(b'\n');
    } else {
        buffer.push(b'=');
        buffer.extend_from_slice(value.as_bytes());
        buffer.push(b'\n');
    }
}

// =============================================================================
/// Sends a single datagram to `/run/systemd/journal/socket` when present on the host.
fn send_journald_datagram(level: LogLevel, message: &str) {
    let socket_path = Path::new(JOURNALD_SOCKET_PATH);
    if !socket_path.exists() {
        return;
    }
    let mut payload = Vec::with_capacity(message.len() + 64);
    append_journal_field(&mut payload, "SYSLOG_IDENTIFIER", SYSLOG_IDENTIFIER);
    append_journal_field(
        &mut payload,
        "PRIORITY",
        &level.syslog_priority().to_string(),
    );
    append_journal_field(&mut payload, "MESSAGE", message);

    if let Ok(sock) = UnixDatagram::unbound() {
        if let Err(_send_err) = sock.send_to(&payload, socket_path) {
            // Non-fatal in minimal containers where journald socket is unreachable;
            // the record is already stored in the in-memory diagnostics ring buffer.
        }
    }
}
