//! Centralized compile-time metadata, FHS 3.0 paths, routes, defaults, and embedded assets.
//!
//! Forms the leaf root of CowHome's Directed Acyclic Graph (DAG). Domain-specific
//! hardware, bauphysics, automation, and analytics constants live in [`super::domain_constants`].

// =============================================================================
/// Package and application identity metadata synchronized with `Cargo.toml` & `CHANGELOG.md`.
pub mod app_meta {
    /// Binary and crate identifier (`Cargo.toml` `package.name`).
    pub const NAME: &str = env!("CARGO_PKG_NAME");
    /// Semantic version (`SemVer 2.0.0`) synchronized with `CHANGELOG.md`.
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
    /// Release date (`YYYY-MM-DD`) synchronized with `CHANGELOG.md`.
    pub const LAST_MODIFIED: &str = "2026-11-24";
    /// Package author(s) from `Cargo.toml`.
    pub const AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
    /// Short package summary displayed in CLI banners.
    pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
    /// SPDX license identifier.
    pub const LICENSE: &str = env!("CARGO_PKG_LICENSE");
    /// Human-readable display title used in CLI headers, TUI menus, and `systemd` units.
    pub const SOFTWARE_NAME: &str = "CowHome";
    /// `systemd-journald` and `syslog` identifier tag.
    pub const SYSLOG_IDENTIFIER: &str = env!("CARGO_PKG_NAME");
}

// =============================================================================
/// Linux Filesystem Hierarchy Standard (`FHS 3.0`) paths derived from `CARGO_PKG_NAME`.
pub mod fhs_paths {
    /// Installed single-binary executable path (`/usr/local/bin/cowhome`).
    pub const BINARY_PATH: &str = concat!("/usr/local/bin/", env!("CARGO_PKG_NAME"));
    /// System-wide configuration directory (`/etc/cowhome`).
    pub const CONFIG_DIR: &str = concat!("/etc/", env!("CARGO_PKG_NAME"));
    /// System-wide TOML configuration file (`/etc/cowhome/config.toml`).
    pub const CONFIG_FILE: &str = concat!("/etc/", env!("CARGO_PKG_NAME"), "/config.toml");
    /// Local configuration filename used when running uninstalled in development mode.
    pub const LOCAL_CONFIG_FILE: &str = "config.toml";
    /// Persistent application state and SQLite directory (`/var/lib/cowhome`).
    pub const STATE_DIR: &str = concat!("/var/lib/", env!("CARGO_PKG_NAME"));
    /// Volatile `tmpfs` runtime directory (`/run/cowhome`).
    pub const RUN_DIR: &str = concat!("/run/", env!("CARGO_PKG_NAME"));
    /// Installed `systemd` service unit file path (`/etc/systemd/system/cowhome.service`).
    pub const SYSTEMD_UNIT_PATH: &str =
        concat!("/etc/systemd/system/", env!("CARGO_PKG_NAME"), ".service");
    /// Registered `systemd` service unit name (`cowhome.service`).
    pub const SYSTEMD_UNIT_NAME: &str = concat!(env!("CARGO_PKG_NAME"), ".service");
    /// Optional XDG desktop entry path (`/usr/share/applications/cowhome.desktop`).
    pub const DESKTOP_ENTRY_PATH: &str = concat!(
        "/usr/share/applications/",
        env!("CARGO_PKG_NAME"),
        ".desktop"
    );
    /// Optional XDG autostart entry cleaned up on uninstall.
    pub const XDG_AUTOSTART_PATH: &str =
        concat!("/etc/xdg/autostart/", env!("CARGO_PKG_NAME"), ".desktop");
    /// Native `systemd-journald` Unix datagram socket.
    pub const JOURNALD_SOCKET_PATH: &str = "/run/systemd/journal/socket";
}

// =============================================================================
/// Canonical HTTP REST API routes, authentication schemes, and security header names.
pub mod route_paths {
    /// Versioned REST API base prefix (`assets/openapi.yaml`).
    pub const API_V1_PREFIX: &str = "/api/v1";
    /// Full Digital Twin snapshot endpoint.
    pub const STATE: &str = "/api/v1/state";
    /// Per-device actuator command endpoint (`axum` path syntax).
    pub const DEVICE_ACTION: &str = "/api/v1/devices/{id}/action";
    /// Virtual software-group collection endpoint.
    pub const GROUPS: &str = "/api/v1/groups";
    /// Individual software-group item endpoint.
    pub const GROUP_ITEM: &str = "/api/v1/groups/{id}";
    /// Column-oriented time-series history endpoint.
    pub const HISTORY: &str = "/api/v1/history";
    /// Outdoor Open-Meteo weather forecast endpoint.
    pub const WEATHER: &str = "/api/v1/weather";
    /// Batch automation configuration endpoint.
    pub const AUTOMATION: &str = "/api/v1/automation";
    /// Single automation feature configuration endpoint.
    pub const AUTOMATION_FEATURE: &str = "/api/v1/automation/{feature}";
    /// FritzBox connection, WAN IP, and WLAN status endpoint.
    pub const SYSTEM_STATUS: &str = "/api/v1/system/status";
    /// SQLite database and host partition storage diagnostics endpoint.
    pub const SYSTEM_STORAGE: &str = "/api/v1/system/storage";
    /// Active TR-064 LAN/WLAN hosts endpoint.
    pub const SYSTEM_HOSTS: &str = "/api/v1/system/hosts";
    /// FritzBox call list and answering-machine status endpoint.
    pub const SYSTEM_CALLS: &str = "/api/v1/system/calls";
    /// Electricity tariff & house-wide energy summary endpoint.
    pub const SYSTEM_ENERGY: &str = "/api/v1/system/energy";
    /// Backwards-compatible alias for [`SYSTEM_ENERGY`].
    pub const SYSTEM_SETTINGS: &str = "/api/v1/system/settings";
    /// Canonical Universal Diagnostics Schema v1.0 endpoint.
    pub const SYSTEM_DIAGNOSTICS: &str = "/api/v1/system/diagnostics";
    /// Backwards-compatible alias for [`SYSTEM_DIAGNOSTICS`].
    pub const SYSTEM_AI_DIAGNOSTICS: &str = "/api/v1/system/ai-diagnostics";
    /// Interactive `sessionStorage`-gated Swagger UI shell.
    pub const DOCS_UI: &str = "/api/v1/docs";
    /// Header-authenticated OpenAPI 3.0.3 YAML specification endpoint.
    pub const OPENAPI_SPEC: &str = "/api/v1/openapi.yaml";

    /// Standard `RFC 9110` `Authorization` request header name.
    pub const HEADER_AUTHORIZATION: &str = "Authorization";
    /// Direct API key request header name (`ApiKeyHeader`).
    pub const HEADER_API_KEY: &str = "X-API-Key";
    /// Optional client creation timestamp header for Last-Write-Wins sync.
    pub const HEADER_CLIENT_TIMESTAMP: &str = "X-Client-Timestamp";
    /// Reverse-proxy client chain header (`OWASP` right-to-left hop evaluation).
    pub const HEADER_FORWARDED_FOR: &str = "X-Forwarded-For";
    /// Single-hop reverse-proxy client IP fallback header.
    pub const HEADER_REAL_IP: &str = "X-Real-IP";
    /// `401 Unauthorized` challenge response header (`RFC 6750`).
    pub const HEADER_WWW_AUTHENTICATE: &str = "WWW-Authenticate";
    /// `RFC 6750` Bearer challenge value returned on `401 Unauthorized`.
    pub const WWW_AUTH_BEARER_REALM: &str = "Bearer realm=\"CowHome\"";
    /// Anti-MIME-sniffing security response header (`OWASP`).
    pub const HEADER_CONTENT_TYPE_OPTIONS: &str = "X-Content-Type-Options";
    /// `X-Frame-Options` anti-clickjacking policy value (`OWASP`).
    pub const HEADER_VAL_FRAME_DENY: &str = "DENY";
    /// `X-Content-Type-Options` anti-MIME-sniffing policy value (`OWASP`).
    pub const HEADER_VAL_NOSNIFF: &str = "nosniff";
    /// `Referrer-Policy` value suppressing outbound Referer headers (`OWASP`).
    pub const HEADER_VAL_NO_REFERRER: &str = "no-referrer";
    /// `Cache-Control` policy preventing sensitive API payload caching (`OWASP`).
    pub const HEADER_VAL_CACHE_NO_STORE: &str = "no-store, no-cache, must-revalidate";
    /// Standard `RFC 6750` Bearer authentication scheme token.
    pub const AUTH_SCHEME_BEARER: &str = "Bearer";
    /// Alternative `ApiKey` authentication scheme token.
    pub const AUTH_SCHEME_APIKEY: &str = "ApiKey";
    /// Forbidden URL query parameter names carrying credentials (`RFC 6750 §2.3`).
    pub const FORBIDDEN_QUERY_KEY_NAMES: &[&str] = &["api_key", "apikey", "access_token", "token"];
}

// =============================================================================
/// Service lifecycle flags, default configuration values, timeouts, and POSIX modes.
pub mod service_defaults {
    /// Provision and manage a `systemd` unit during `install` / `update`.
    pub const CREATE_SERVICE: bool = true;
    /// Headless server daemon – no XDG `.desktop` entry provisioned.
    pub const ENABLE_DESKTOP_ENTRY: bool = false;
    /// `systemd` service type (`Type=simple`).
    pub const SERVICE_TYPE: &str = "simple";
    /// Unprivileged system user executing `cowhome.service`.
    pub const SERVICE_USER: &str = "cowhome";
    /// Primary system group executing `cowhome.service`.
    pub const SERVICE_GROUP: &str = "cowhome";
    /// Automatically create `cowhome` user/group via `useradd --system` during `install`.
    pub const SERVICE_CREATE_USER: bool = true;
    /// Default login shell assigned to created system service accounts.
    pub const SERVICE_USER_SHELL: &str = "/usr/sbin/nologin";
    /// Optional space-separated supplementary groups (empty = omitted).
    pub const SERVICE_SUPPLEMENTARY_GROUPS: &str = "";
    /// `UMask=` directive in the rendered unit (`0027` keeps DB/WAL private to group).
    pub const SERVICE_UMASK: &str = "0027";
    /// `ExecStart=` subcommand appended after `BINARY_PATH`.
    pub const SERVICE_EXEC_SUBCOMMAND: &str = "daemon";
    /// Optional `ExecStop=` command (empty = `systemd` sends `SIGTERM` directly).
    pub const SERVICE_EXEC_STOP: &str = "";
    /// `systemd` restart policy.
    pub const SERVICE_RESTART: &str = "always";
    /// Delay in seconds before `systemd` restarts a failed daemon.
    pub const SERVICE_RESTART_SEC: u64 = 5;
    /// `systemd` startup timeout in seconds.
    pub const SERVICE_TIMEOUT_START_SEC: u64 = 60;
    /// `systemd` graceful stop timeout in seconds before `SIGKILL`.
    pub const SERVICE_TIMEOUT_STOP_SEC: u64 = 15;
    /// Optional `PartOf=` unit binding (empty = omitted).
    pub const SERVICE_PART_OF: &str = "";
    /// Optional `Environment=` key-value pairs (empty = omitted).
    pub const SERVICE_ENVIRONMENT: &str = "";
    /// Optional additional `ReadWritePaths=` entries (empty = omitted).
    pub const SERVICE_READ_WRITE_PATHS: &str = "";
    /// `NoNewPrivileges=` hardening setting.
    pub const SERVICE_NO_NEW_PRIVILEGES: &str = "true";
    /// `PrivateTmp=` hardening setting.
    pub const SERVICE_PRIVATE_TMP: &str = "true";
    /// `PrivateDevices=` setting.
    pub const SERVICE_PRIVATE_DEVICES: &str = "true";
    /// `ProtectSystem=` sandboxing level (`full` keeps `/usr`, `/boot`, `/etc` read-only).
    pub const SERVICE_PROTECT_SYSTEM: &str = "full";
    /// `ProtectHome=` sandboxing.
    pub const SERVICE_PROTECT_HOME: &str = "true";

    /// Default log verbosity threshold.
    pub const DEFAULT_LOG_LEVEL: &str = "INFO";
    /// Default ANSI color preference before TTY and `NO_COLOR` checks.
    pub const DEFAULT_CONSOLE_COLORS: bool = true;
    /// Default HTTP bind address (`127.0.0.1` for local reverse proxy).
    pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";
    /// Default HTTP listen port.
    pub const DEFAULT_SERVER_PORT: u16 = 8010;
    /// Default external base URL.
    pub const DEFAULT_SERVER_BASE_URL: &str = "https://home.libracow.com";
    /// Maximum allowed HTTP request body size (`1 MiB`).
    pub const MAX_HTTP_BODY_BYTES: usize = 1_048_576;
    /// Default placeholder API key written into unconfigured `config.default.toml`.
    pub const DEFAULT_PLACEHOLDER_API_KEY: &str = "changeme-super-secret-key";
    /// Default trusted loopback proxies allowed to supply `X-Forwarded-For` / `X-Real-IP`.
    pub const DEFAULT_TRUSTED_PROXIES: &[&str] = &["127.0.0.1", "::1"];
    /// Minimum character length required for a secure API key (`OWASP ASVS v4.0`).
    pub const MIN_API_KEY_LEN: usize = 16;
    /// Known insecure placeholder API keys rejected during fail-closed validation.
    pub const WEAK_PLACEHOLDER_KEYS: &[&str] = &[
        DEFAULT_PLACEHOLDER_API_KEY,
        "your-api-key-here",
        "your-api-secret-here",
        "your_super_secret_api_key",
        "your_secret_api_key",
        "your_strong_random_api_key",
        "changeme",
        "change-me",
        "secret",
        "admin",
        "password",
        "1234567890123456",
    ];

    /// Default sliding-window rate limiter activation flag.
    pub const DEFAULT_RATE_LIMIT_ENABLED: bool = true;
    /// Sliding-window burst period in seconds (`1` minute) for per-IP request rate limiting.
    pub const RATE_LIMIT_MINUTE_WINDOW_SECS: u64 = 60;
    /// Interval in seconds (`5` minutes) between expired rate-limiter bucket cleanups.
    pub const RATE_LIMIT_CLEANUP_SECS: u64 = 300;
    /// Default lockout window in seconds (`1` hour) for failed API key attempts.
    pub const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 3600;
    /// Default maximum failed authentication attempts per IP within `window`.
    pub const DEFAULT_MAX_LOGIN_ATTEMPTS: u32 = 5;
    /// Default maximum total HTTP requests per minute per IP.
    pub const DEFAULT_MAX_REQUESTS_PER_MIN: u32 = 120;

    /// Default FritzBox hostname.
    pub const DEFAULT_FRITZBOX_HOST: &str = "fritz.box";
    /// Default FritzBox polling interval in seconds.
    pub const DEFAULT_FRITZBOX_POLL_SECS: u64 = 60;
    /// Default outbound HTTP timeout in seconds for FritzBox AHA/TR-064 requests.
    pub const DEFAULT_FRITZBOX_TIMEOUT_SECS: u64 = 15;

    /// Default background daemon enablement flag.
    pub const DEFAULT_DAEMON_ENABLED: bool = true;
    /// Default transient failure retry count per cycle.
    pub const DEFAULT_MAX_RETRIES: u32 = 3;
    /// Default delay in seconds between transient retry attempts.
    pub const DEFAULT_RETRY_DELAY_SECS: u64 = 5;
    /// Initial startup delay in seconds (`2 s`) before background worker loops begin polling.
    pub const WORKER_STARTUP_DELAY_SECS: u64 = 2;
    /// Minimum sleep duration in seconds (`1 s`) between consecutive background worker cycles.
    pub const WORKER_MIN_SLEEP_SECS: u64 = 1;
    /// Default relative SQLite filename inside `STATE_DIR`.
    pub const DEFAULT_DB_FILENAME: &str = "cowhome.db";

    /// Public IPv4 target used for Setup TUI internet connectivity probes.
    pub const NET_PROBE_IP: &str = "1.1.1.1";
    /// HTTP 204 fallback URL used when ICMP `ping` is blocked on the network.
    pub const NET_PROBE_URL: &str = "https://connectivitycheck.gstatic.com/generate_204";
    /// Per-attempt ICMP ping timeout in seconds.
    pub const NET_PING_TIMEOUT_SECS: u64 = 2;
    /// HTTP fallback connectivity probe timeout in seconds.
    pub const NET_HTTP_TIMEOUT_SECS: u64 = 5;
    /// Timeout in seconds (`5 s`) for local CLI-to-daemon REST API probes (`status` / break-glass).
    pub const LOCAL_API_TIMEOUT_SECS: u64 = 5;
    /// Default timeout in seconds for non-interactive system subprocesses.
    pub const SUBPROCESS_TIMEOUT_SECS: u64 = 30;
    /// Timeout in seconds for interactive `sudo` authentication.
    pub const SUDO_TIMEOUT_SECS: u64 = 60;
    /// Timeout in seconds (`5` minutes) for system package manager installation commands.
    pub const PKG_INSTALL_TIMEOUT_SECS: u64 = 300;
    /// Timeout in seconds (`6` minutes) for elevated `sudo <name> setup ...` child invocations.
    pub const ELEVATED_SETUP_TIMEOUT_SECS: u64 = 360;
    /// Timeout in seconds (`30` minutes) for interactive `sudo <name> config` TUI sessions.
    pub const ELEVATED_CONFIG_TIMEOUT_SECS: u64 = 1800;
    /// Polling interval in milliseconds for bounded subprocesses and signal waits.
    pub const SUBPROCESS_POLL_MS: u64 = 25;

    /// Standard public directory permission mode (`rwxr-xr-x`).
    pub const DIR_MODE: u32 = 0o755;
    /// Restricted service directory permission mode (`rwxr-x---`).
    pub const PRIVATE_DIR_MODE: u32 = 0o750;
    /// Public unit permission mode (`rw-r--r--`).
    pub const PUBLIC_FILE_MODE: u32 = 0o644;
    /// Group-readable service configuration permission mode (`rw-r-----`, `root:cowhome`).
    pub const GROUP_CONFIG_MODE: u32 = 0o640;
    /// Owner-only secret/backup file permission mode (`rw-------`).
    pub const PRIVATE_FILE_MODE: u32 = 0o600;
    /// Installed executable permission mode (`rwxr-xr-x`).
    pub const BINARY_FILE_MODE: u32 = 0o755;

    /// Masked secret prefix rendered in CLI status views and sanitized logs.
    pub const MASKED_SECRET_PREFIX: &str = "••••••••";
    /// Number of trailing characters preserved when masking long API keys.
    pub const MASKED_SECRET_SUFFIX_LEN: usize = 4;
    /// Canonical replacement value used in JSON and URI secret redaction.
    pub const REDACTED_VALUE: &str = "**REDACTED**";
}

// =============================================================================
/// Flash-friendly SQLite `WAL` wear-leveling constants (`rusqlite` bundled).
pub mod database_defaults {
    /// Baseline SQLite `PRAGMA user_version` schema version.
    pub const DB_SCHEMA_VERSION: i32 = 1;
    /// Write-Ahead Logging mode for concurrent readers and sequential flash writes.
    pub const JOURNAL_MODE: &str = "WAL";
    /// Crash-safe fsync policy paired with `WAL` (`NORMAL` avoids double-fsync wear).
    pub const SYNCHRONOUS: &str = "NORMAL";
    /// Lock contention busy-wait timeout in milliseconds (`30` seconds).
    pub const BUSY_TIMEOUT_MS: u32 = 30_000;
    /// Upper bound (`64 MiB`) on `-wal` file growth after checkpoints.
    pub const JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864;
    /// Page cache target (`16 MiB` in RAM via negative kibibytes).
    pub const CACHE_SIZE_KIB: i32 = -16_384;
    /// Memory-mapped read window (`256 MiB`) accelerating history scans.
    pub const MMAP_SIZE_BYTES: i64 = 268_435_456;
    /// Keep temporary B-trees and sort tables strictly in RAM (`0 B` flash wear).
    pub const TEMP_STORE: &str = "MEMORY";
    /// Enforce relational foreign-key constraints on every connection.
    pub const FOREIGN_KEYS: bool = true;
    /// Number of pooled `WAL` connections maintained per `Database` instance.
    pub const DEFAULT_POOL_SIZE: usize = 5;
    /// Maximum wait time in seconds (`10 s`) when checking out a pooled SQLite connection.
    pub const POOL_CHECKOUT_TIMEOUT_SECS: u64 = 10;
}

// =============================================================================
/// Universal Diagnostics Schema v1.0 thresholds, alert codes, and ring-buffer limits.
pub mod diagnostics_defaults {
    /// Universal Diagnostics Schema version string.
    pub const SCHEMA_VERSION: &str = "1.0";
    /// Maximum log entries retained in the in-memory `Logger` fallback ring buffer.
    pub const RING_BUFFER_CAPACITY: usize = 200;
    /// Default number of recent log lines included in a diagnostics snapshot.
    pub const DEFAULT_LOG_LIMIT: usize = 25;
    /// Maximum allowed log lines per diagnostics query.
    pub const MAX_LOG_LIMIT: usize = 200;
    /// Disk usage percentage triggering a `warn` health check (`ISA-18.2`).
    pub const DISK_WARN_PERCENT: f64 = 85.0;
    /// Disk usage percentage triggering a `fail` health check (`ISA-18.2`).
    pub const DISK_FAIL_PERCENT: f64 = 95.0;
    /// Default historical aggregation window token (`24h` in CowHome).
    pub const DEFAULT_RANGE: &str = "24h";
    /// Supported historical time-range filter tokens (`assets/openapi.yaml` & history ranges).
    pub const SUPPORTED_RANGES: &[&str] = &["24h", "7d", "30d", "90d", "180d", "365d", "1y", "all"];
    /// Canonical alert code when a Smart Home device is unreachable.
    pub const ALERT_DEVICE_OFFLINE: &str = "device_offline";
    /// Canonical alert code when a battery-operated device reports low charge.
    pub const ALERT_BATTERY_LOW: &str = "battery_low";
    /// Prefix for AVM radiator valve error codes (`valve_error_1`..`valve_error_6`).
    pub const ALERT_VALVE_ERROR_PREFIX: &str = "valve_error_";
    /// Canonical alert code when an internal FritzBox offset (`!= 0.0 °C`) is active.
    pub const ALERT_FRITZ_OFFSET_ACTIVE: &str = "fritz_offset_active";
    /// Canonical alert code when Open-Meteo weather fetching is degraded.
    pub const ALERT_WEATHER_DEGRADED: &str = "weather_service_degraded";
    /// Canonical alert code when database health check is non-`pass`.
    pub const ALERT_DB_UNHEALTHY: &str = "database_unhealthy";
    /// Canonical alert code when storage free space crosses warn/fail thresholds.
    pub const ALERT_STORAGE_LOW: &str = "storage_space_low";
    /// Canonical alert code when `systemd` unit is not `active`.
    pub const ALERT_SERVICE_DEGRADED: &str = "systemd_service_degraded";
    /// Substring tokens identifying secret keys during recursive JSON redaction.
    pub const SENSITIVE_KEY_TOKENS: &[&str] = &[
        "api_key",
        "apikey",
        "api_secret",
        "secret",
        "password",
        "passwd",
        "token",
        "private_key",
        "encryption_key",
        "fernet_key",
        "credential",
        "authorization",
    ];
}

// =============================================================================
/// Static template and documentation files embedded into the single binary at compile time.
pub mod embedded_assets {
    /// Default `/etc/cowhome/config.toml` template.
    pub const DEFAULT_CONFIG_TOML: &str = include_str!("../../assets/config.default.toml");
    /// Hardened `systemd` service unit template (`assets/cowhome.service`).
    pub const SYSTEMD_UNIT_TEMPLATE: &str = include_str!("../../assets/cowhome.service");
    /// Canonical OpenAPI 3.0.3 YAML specification (`assets/openapi.yaml`).
    pub const OPENAPI_SPEC_YAML: &str = include_str!("../../assets/openapi.yaml");
    /// Zero-secret Swagger UI HTML shell (`assets/swagger.html`).
    pub const SWAGGER_UI_HTML: &str = include_str!("../../assets/swagger.html");
}

// =============================================================================
/// CLI symbols, banner dimensions, pagination defaults, ANSI codes, and POSIX exit codes.
pub mod cli_style {
    /// Success indicator symbol.
    pub const SYM_OK: &str = "✓";
    /// Warning and error indicator symbol.
    pub const SYM_ERR: &str = "✗";
    /// Informational indicator symbol.
    pub const SYM_INFO: &str = "ℹ";
    /// Decorative banner width (`63` columns).
    pub const BANNER_WIDTH: usize = 63;
    /// Left-aligned status label width (`15` chars including trailing colon).
    pub const STATUS_LABEL_WIDTH: usize = 15;
    /// Default page size for interactive CLI list pagination.
    pub const DEFAULT_PAGE_SIZE: usize = 10;
    /// ANSI clear-screen and cursor-home sequence for interactive TUI menus.
    pub const ANSI_CLEAR_SCREEN: &str = "\x1b[H\x1b[2J";
    /// ANSI green escape sequence.
    pub const ANSI_GREEN: &str = "\x1b[0;32m";
    /// ANSI bold yellow escape sequence.
    pub const ANSI_YELLOW: &str = "\x1b[1;33m";
    /// ANSI red escape sequence.
    pub const ANSI_RED: &str = "\x1b[0;31m";
    /// ANSI blue escape sequence.
    pub const ANSI_BLUE: &str = "\x1b[0;34m";
    /// ANSI cyan escape sequence.
    pub const ANSI_CYAN: &str = "\x1b[0;36m";
    /// ANSI bold white escape sequence.
    pub const ANSI_WHITE: &str = "\x1b[1;37m";
    /// ANSI bold attribute escape sequence.
    pub const ANSI_BOLD: &str = "\x1b[1m";
    /// ANSI dim attribute escape sequence.
    pub const ANSI_DIM: &str = "\x1b[2m";
    /// ANSI reset escape sequence.
    pub const ANSI_RESET: &str = "\x1b[0m";
    /// Process exit code indicating clean completion (`0`).
    pub const EXIT_SUCCESS: i32 = 0;
    /// Process exit code indicating runtime, validation, or setup failure (`1`).
    pub const EXIT_FAILURE: i32 = 1;
    /// Process exit code indicating invalid CLI usage (`2`, `POSIX.1-2017`).
    pub const EXIT_USAGE: i32 = 2;
    /// Process exit code indicating interrupted execution via `SIGINT` (`130`).
    pub const EXIT_SIGINT: i32 = 130;
}
