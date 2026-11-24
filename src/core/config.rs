//! Typed TOML configuration models, path resolution, and resilient runtime loading.
//!
//! Resolves `/etc/cowhome/config.toml` (installed) vs `./config.toml` (local dev mode)
//! and merges missing optional keys safely against compile-time defaults.
//! Strict validation and atomic saving live in [`crate::core::config_io`].

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::core::config_io::{format_config_toml, save_config};
pub use crate::core::config_schema::{parse_strict_config_toml, validate_config_file};
use crate::core::constants::fhs_paths::{
    BINARY_PATH, CONFIG_DIR, CONFIG_FILE, LOCAL_CONFIG_FILE, STATE_DIR,
};
use crate::core::constants::service_defaults::{
    DEFAULT_CONSOLE_COLORS, DEFAULT_DAEMON_ENABLED, DEFAULT_DB_FILENAME, DEFAULT_FRITZBOX_HOST,
    DEFAULT_FRITZBOX_POLL_SECS, DEFAULT_LOG_LEVEL, DEFAULT_MAX_LOGIN_ATTEMPTS,
    DEFAULT_MAX_REQUESTS_PER_MIN, DEFAULT_PLACEHOLDER_API_KEY, DEFAULT_RATE_LIMIT_ENABLED,
    DEFAULT_RATE_LIMIT_WINDOW_SECS, DEFAULT_SERVER_BASE_URL, DEFAULT_SERVER_HOST,
    DEFAULT_SERVER_PORT, DEFAULT_TRUSTED_PROXIES, GROUP_CONFIG_MODE, MIN_API_KEY_LEN,
    PRIVATE_FILE_MODE,
};
use crate::core::domain_constants::automation::{
    automation_defaults::{ENGINE_INTERVAL_SECS, VACATION_MODE_DEFAULT},
    weather_defaults::{
        FORECAST_DAYS, LATITUDE, LONGITUDE, MAX_FORECAST_DAYS, TIMEZONE, UPDATE_INTERVAL_SECS,
    },
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::LogLevel;
use crate::core::security::{PathPresence, probe_path_presence, read_file_nofollow};
use crate::core::validation::{
    is_api_key_secure, parse_ip_or_cidr, validate_coordinates, validate_host_or_ip,
    validate_http_endpoint, validate_safe_path, validate_timezone,
};

// =============================================================================
/// Top-level typed runtime configuration matching `assets/config.default.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Logging verbosity and console color settings (`[debug]`).
    pub debug: DebugConfig,
    /// HTTP REST server bind address, port, and public base URL (`[server]`).
    pub server: ServerConfig,
    /// Inbound REST API authentication key and reverse-proxy trust (`[api]`).
    pub api: ApiConfig,
    /// Two-tier sliding-window DoS & auth lockout settings (`[rate_limit]`).
    pub rate_limit: RateLimitConfig,
    /// FritzBox AHA & TR-064 gateway credentials and poll interval (`[fritzbox]`).
    pub fritzbox: FritzboxConfig,
    /// Foreground daemon enablement (`[daemon]`).
    pub daemon: DaemonConfig,
    /// Persistent SQLite path settings (`[database]`).
    pub database: DatabaseConfig,
    /// Open-Meteo location, IANA timezone, and forecast horizon (`[weather]`).
    pub weather: WeatherConfig,
    /// `RulesEngine` evaluation interval and vacation override (`[automation]`).
    pub automation: AutomationConfig,
}

impl AppConfig {
    /// Validates all configuration bounds, network literals, coordinates, and API key policy.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if any field violates security or domain bounds.
    pub fn validate(&self) -> AppResult<()> {
        self.validate_network_and_security()?;
        self.validate_hardware_and_weather()
    }

    /// Validates `[server]`, `[api]`, and `[rate_limit]` bounds.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid bind host, port `0`, weak key, or bad CIDR.
    fn validate_network_and_security(&self) -> AppResult<()> {
        validate_host_or_ip(&self.server.host)?;
        if self.server.port == 0 {
            return Err(AppError::Validation("server.port must be >= 1".into()));
        }
        validate_http_endpoint(&self.server.base_url)?;
        if !is_api_key_secure(&self.api.api_key) {
            return Err(AppError::Validation(format!(
                "api.api_key must be >= {MIN_API_KEY_LEN} printable ASCII chars (no placeholders)"
            )));
        }
        for entry in &self.api.trusted_proxies {
            parse_ip_or_cidr(entry)?;
        }
        if self.rate_limit.window == 0
            || self.rate_limit.max_login_attempts == 0
            || self.rate_limit.max_requests_per_minute_per_ip == 0
        {
            return Err(AppError::Validation(
                "rate_limit window and attempt thresholds must be >= 1".into(),
            ));
        }
        Ok(())
    }

    /// Validates `[fritzbox]`, `[database]`, `[weather]`, and `[automation]` bounds.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid FritzBox host, coordinates, timezone, or path.
    fn validate_hardware_and_weather(&self) -> AppResult<()> {
        validate_host_or_ip(&self.fritzbox.host)?;
        if self.fritzbox.poll_interval == 0 {
            return Err(AppError::Validation(
                "fritzbox.poll_interval must be >= 1s".into(),
            ));
        }
        validate_safe_path(&self.database.db_path, "database.db_path")?;
        validate_coordinates(self.weather.latitude, self.weather.longitude)?;
        validate_timezone(&self.weather.timezone)?;
        if self.weather.update_interval == 0
            || !(1..=MAX_FORECAST_DAYS).contains(&self.weather.forecast_days)
        {
            return Err(AppError::Validation(format!(
                "weather.update_interval >= 1s and forecast_days in 1..={MAX_FORECAST_DAYS} required"
            )));
        }
        if self.automation.interval == 0 {
            return Err(AppError::Validation(
                "automation.interval must be >= 1s".into(),
            ));
        }
        Ok(())
    }

    /// Resolves `database.db_path` against `STATE_DIR` (or `.` in local uninstalled mode).
    #[must_use]
    pub fn resolved_database_path(&self) -> PathBuf {
        let candidate = Path::new(&self.database.db_path);
        if candidate.is_absolute() {
            return candidate.to_path_buf();
        }
        if is_installed_context() {
            Path::new(STATE_DIR).join(candidate)
        } else {
            PathBuf::from(candidate)
        }
    }

    /// Returns `0640` (`root:cowhome` inside `0750` `/etc/cowhome`) or `0600` for local files.
    #[must_use]
    pub fn target_file_mode(&self, target_path: &Path) -> u32 {
        if target_path.starts_with("/etc/") || target_path.starts_with(CONFIG_DIR) {
            GROUP_CONFIG_MODE
        } else {
            PRIVATE_FILE_MODE
        }
    }
}

// =============================================================================
/// Logging threshold and ANSI color configuration (`[debug]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Minimum log severity forwarded to `journald` and the diagnostics ring buffer.
    pub level: LogLevel,
    /// Whether ANSI escape codes are allowed when attached to a TTY.
    pub console_colors: bool,
}

impl Default for DebugConfig {
    /// Returns default logging verbosity (`INFO`) and TTY color preferences.
    fn default() -> Self {
        Self {
            level: LogLevel::parse_strict(DEFAULT_LOG_LEVEL).unwrap_or(LogLevel::Info),
            console_colors: DEFAULT_CONSOLE_COLORS,
        }
    }
}

// =============================================================================
/// HTTP server bind address, port, and public base URL (`[server]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// IPv4/IPv6 bind address or hostname (`127.0.0.1` or `0.0.0.0`).
    pub host: String,
    /// TCP port on which `axum` listens (`8010`).
    pub port: u16,
    /// External base URL advertised to mobile clients.
    pub base_url: String,
}

impl Default for ServerConfig {
    /// Returns default local bind settings (`127.0.0.1:8010`).
    fn default() -> Self {
        Self {
            host: DEFAULT_SERVER_HOST.to_owned(),
            port: DEFAULT_SERVER_PORT,
            base_url: DEFAULT_SERVER_BASE_URL.to_owned(),
        }
    }
}

// =============================================================================
/// Inbound REST API security settings (`[api]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Bearer / `X-API-Key` authentication secret (`>= 16` printable ASCII chars).
    pub api_key: String,
    /// Reverse proxy IPs or CIDRs allowed to set `X-Forwarded-For` / `X-Real-IP`.
    pub trusted_proxies: Vec<String>,
}

impl Default for ApiConfig {
    /// Returns fail-closed placeholder API key and loopback trusted proxies.
    fn default() -> Self {
        Self {
            api_key: DEFAULT_PLACEHOLDER_API_KEY.to_owned(),
            trusted_proxies: DEFAULT_TRUSTED_PROXIES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

// =============================================================================
/// Sliding-window DoS and authentication brute-force protection (`[rate_limit]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Enable IP request rate limiting and failed-login lockouts.
    pub enabled: bool,
    /// Sliding lockout window in seconds (`3600` = `1` hour).
    pub window: u64,
    /// Maximum failed authentication attempts per IP within `window`.
    pub max_login_attempts: u32,
    /// Maximum total HTTP requests per minute per IP.
    pub max_requests_per_minute_per_ip: u32,
}

impl Default for RateLimitConfig {
    /// Returns default two-tier sliding-window rate limits (`120/min`, `5` logins/`3600 s`).
    fn default() -> Self {
        Self {
            enabled: DEFAULT_RATE_LIMIT_ENABLED,
            window: DEFAULT_RATE_LIMIT_WINDOW_SECS,
            max_login_attempts: DEFAULT_MAX_LOGIN_ATTEMPTS,
            max_requests_per_minute_per_ip: DEFAULT_MAX_REQUESTS_PER_MIN,
        }
    }
}

// =============================================================================
/// FritzBox AHA & TR-064 gateway connection settings (`[fritzbox]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FritzboxConfig {
    /// Hostname or IP address of the FritzBox (`fritz.box` or `192.168.178.1`).
    pub host: String,
    /// FritzBox Smart Home / TR-064 username.
    pub username: String,
    /// FritzBox Smart Home / TR-064 password.
    pub password: String,
    /// Background polling interval in seconds.
    pub poll_interval: u64,
}

impl Default for FritzboxConfig {
    /// Returns default FritzBox endpoint (`fritz.box`, `30 s` poll interval).
    fn default() -> Self {
        Self {
            host: DEFAULT_FRITZBOX_HOST.to_owned(),
            username: String::new(),
            password: String::new(),
            poll_interval: DEFAULT_FRITZBOX_POLL_SECS,
        }
    }
}

// =============================================================================
/// Background daemon enablement settings (`[daemon]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Execute background polling and automation workers in daemon mode.
    pub enabled: bool,
}

impl Default for DaemonConfig {
    /// Returns default background worker enablement (`true`).
    fn default() -> Self {
        Self {
            enabled: DEFAULT_DAEMON_ENABLED,
        }
    }
}

// =============================================================================
/// Persistent SQLite storage configuration (`[database]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Relative SQLite filename inside `STATE_DIR` or explicit absolute path.
    pub db_path: String,
}

impl Default for DatabaseConfig {
    /// Returns default SQLite database filename (`cowhome.db`).
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_FILENAME.to_owned(),
        }
    }
}

// =============================================================================
/// Open-Meteo coordinates, home IANA timezone, and forecast horizon (`[weather]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    /// Home WGS-84 latitude (`-90.0`..=`90.0`).
    pub latitude: f64,
    /// Home WGS-84 longitude (`-180.0`..=`180.0`).
    pub longitude: f64,
    /// Home IANA timezone (`"Europe/Berlin"` or `"auto"`).
    pub timezone: String,
    /// Weather refresh interval in seconds (`3600`).
    pub update_interval: u64,
    /// Number of forecast days requested from Open-Meteo (`1`..=`16`).
    pub forecast_days: u8,
}

impl Default for WeatherConfig {
    /// Returns default coordinates, `Europe/Berlin` timezone, and `5`-day horizon.
    fn default() -> Self {
        Self {
            latitude: LATITUDE,
            longitude: LONGITUDE,
            timezone: TIMEZONE.to_owned(),
            update_interval: UPDATE_INTERVAL_SECS,
            forecast_days: FORECAST_DAYS,
        }
    }
}

// =============================================================================
/// `RulesEngine` scheduling and house-wide vacation override (`[automation]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationConfig {
    /// Evaluation interval in seconds (`60`).
    pub interval: u64,
    /// Global frost-protection vacation override flag.
    pub vacation_mode: bool,
}

impl Default for AutomationConfig {
    /// Returns default `RulesEngine` evaluation interval (`60 s`) and `vacation_mode = false`.
    fn default() -> Self {
        Self {
            interval: ENGINE_INTERVAL_SECS,
            vacation_mode: VACATION_MODE_DEFAULT,
        }
    }
}

// =============================================================================
/// Resolves the active configuration path (`--file`, `CONFIG_FILE` env, `/etc`, or local).
#[must_use]
pub fn resolve_config_path(explicit_file: Option<&Path>) -> PathBuf {
    if let Some(custom) = explicit_file {
        return custom.to_path_buf();
    }
    if let Some(env_cfg) = std::env::var_os("CONFIG_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(env_cfg);
    }
    let sys_cfg = Path::new(CONFIG_FILE);
    let local_cfg = Path::new(LOCAL_CONFIG_FILE);
    let sys_presence = probe_path_presence(sys_cfg);
    if is_installed_context()
        || sys_presence == PathPresence::Accessible
        || (sys_presence == PathPresence::PermissionDenied && !local_cfg.exists())
    {
        return sys_cfg.to_path_buf();
    }
    local_cfg.to_path_buf()
}

// =============================================================================
/// Returns `true` when running from `/usr/local/bin/cowhome`.
fn is_installed_context() -> bool {
    std::env::current_exe()
        .map(|exe| exe == Path::new(BINARY_PATH))
        .unwrap_or(false)
}

// =============================================================================
/// Loads runtime configuration with forward-compatible defaults for missing optional keys.
///
/// # Errors
/// Returns [`AppError::Auth`] if `0750`/`0640` permissions block reading an installed config,
/// or [`AppError`] if an explicit `--file` is missing, a symlink, or invalid TOML.
pub fn load_runtime_config(explicit_file: Option<&Path>) -> AppResult<(AppConfig, PathBuf)> {
    let path = resolve_config_path(explicit_file);
    match probe_path_presence(&path) {
        PathPresence::Missing => {
            if explicit_file.is_some() {
                return Err(AppError::Config(format!(
                    "Configuration file '{}' does not exist",
                    path.display()
                )));
            }
            return Ok((AppConfig::default(), path));
        }
        PathPresence::PermissionDenied => {
            return Err(AppError::Auth(format!(
                "Permission denied reading '{}' (run with 'sudo')",
                path.display()
            )));
        }
        PathPresence::Accessible => {}
    }
    let raw_text = read_file_nofollow(&path).map_err(|err| match err {
        AppError::Io { ref source, .. } if source.kind() == ErrorKind::PermissionDenied => {
            AppError::Auth(format!(
                "Permission denied reading '{}' (run with 'sudo')",
                path.display()
            ))
        }
        other => other,
    })?;
    let cfg: AppConfig = toml::from_str(&raw_text).map_err(|err| {
        AppError::Config(format!(
            "Invalid TOML syntax in '{}': {err}",
            path.display()
        ))
    })?;
    Ok((cfg, path))
}

// =============================================================================
/// Loads runtime config for read-only discovery (`status` / `Runtime` TUI), returning
/// `(config, path, permission_locked)` instead of failing when `/etc/cowhome` is `0750`.
///
/// # Errors
/// Returns [`AppError`] if an explicit `--file` fails or TOML syntax is malformed.
pub fn load_runtime_config_lenient(
    explicit_file: Option<&Path>,
) -> AppResult<(AppConfig, PathBuf, bool)> {
    match load_runtime_config(explicit_file) {
        Ok((cfg, path)) => Ok((cfg, path, false)),
        Err(AppError::Auth(_)) if explicit_file.is_none() => {
            Ok((AppConfig::default(), resolve_config_path(None), true))
        }
        Err(err) => Err(err),
    }
}
