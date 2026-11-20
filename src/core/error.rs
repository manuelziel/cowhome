//! Central application error hierarchy and deterministic POSIX exit-code mapping.
//!
//! Defines [`AppError`] and [`AppResult`] so every fallible operation across
//! configuration, database, security, services, setup, HTTP handlers, and CLI
//! propagates structured errors without `.unwrap()` or silent error suppression.

use crate::core::constants::cli_style::{EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE};

// =============================================================================
/// Project-wide result alias backed by [`AppError`].
pub type AppResult<T> = Result<T, AppError>;

// =============================================================================
/// Structured domain error covering all failure modes in CowHome.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Configuration file loading, parsing, serialization, or persistence failure.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Input, schema, hostname/IP, URL, offset, or path validation failure (`HTTP 400`).
    #[error("Validation error: {0}")]
    Validation(String),

    /// API key, FritzBox login, `sudo`, or POSIX account authentication failure (`HTTP 401`).
    #[error("Authentication failed: {0}")]
    Auth(String),

    /// Requested device, software group, or resource does not exist (`HTTP 404`).
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// Single-Authority group lock, duplicate membership, or calibration conflict (`HTTP 409`).
    #[error("Conflict: {0}")]
    Conflict(String),

    /// Sliding-window IP rate limit or failed-login lockout exceeded (`HTTP 429`).
    #[error("Rate limit exceeded: {0}")]
    RateLimit(String),

    /// SQLite query, transaction, lock timeout, or schema error (`HTTP 500` / `503`).
    #[error("Database error: {0}")]
    Database(String),

    /// Security boundary violation (e.g., `CWE-59` symbolic link rejection).
    #[error("Security violation: {0}")]
    Security(String),

    /// Domain service, FritzBox gateway, or background worker error (`HTTP 502`).
    #[error("Service error: {0}")]
    Service(String),

    /// System installation, package dependency, user, or `systemd` lifecycle failure.
    #[error("Setup error: {0}")]
    Setup(String),

    /// Invalid command-line flags or unknown subcommands (`POSIX` exit code `2`).
    #[error("Invalid CLI usage: {0}")]
    Usage(String),

    /// Contextualized filesystem or operating-system I/O error.
    #[error("{context}: {source}")]
    Io {
        /// Human-readable description of the attempted operation.
        context: String,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },

    /// Graceful user cancellation (`EOF` / `Ctrl+D`, `SIGINT`, or declined prompt).
    #[error("Operation cancelled by user")]
    Cancelled,
}

impl AppError {
    /// Wraps a [`std::io::Error`] with explicit operational context.
    #[must_use]
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Maps this error variant to its deterministic POSIX process exit code.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Cancelled => EXIT_SUCCESS,
            Self::Usage(_) => EXIT_USAGE,
            Self::Config(_)
            | Self::Validation(_)
            | Self::Auth(_)
            | Self::NotFound(_)
            | Self::Conflict(_)
            | Self::RateLimit(_)
            | Self::Database(_)
            | Self::Security(_)
            | Self::Service(_)
            | Self::Setup(_)
            | Self::Io { .. } => EXIT_FAILURE,
        }
    }
}

impl From<rusqlite::Error> for AppError {
    /// Converts a `rusqlite::Error` into [`AppError::Database`].
    fn from(err: rusqlite::Error) -> Self {
        Self::Database(err.to_string())
    }
}
