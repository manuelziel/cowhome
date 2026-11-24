//! Strict TOML schema validation (`deny_unknown_fields`) for `cowhome validate`.
//!
//! Separated from [`crate::core::config`] and [`crate::core::config_io`] so schema
//! deserialization, runtime models, and atomic persistence each remain compact (`< 350` lines).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::config::{
    ApiConfig, AppConfig, AutomationConfig, FritzboxConfig, RateLimitConfig, ServerConfig,
    WeatherConfig, resolve_config_path,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::LogLevel;
use crate::core::security::{PathPresence, probe_path_presence, read_file_nofollow};

// =============================================================================
/// Strict top-level TOML schema rejecting misspelled or unknown tables (`cowhome validate`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictConfigSchema {
    /// Optional `[debug]` table.
    debug: Option<StrictDebug>,
    /// Optional legacy `[app]` metadata table accepted for backwards compatibility.
    #[allow(dead_code)]
    app: Option<StrictLegacyApp>,
    /// Optional `[server]` table.
    server: Option<StrictServer>,
    /// Optional `[api]` table.
    api: Option<StrictApi>,
    /// Optional `[rate_limit]` table.
    rate_limit: Option<StrictRateLimit>,
    /// Optional `[fritzbox]` table.
    fritzbox: Option<StrictFritzbox>,
    /// Optional `[daemon]` table.
    daemon: Option<StrictDaemon>,
    /// Optional `[database]` table.
    database: Option<StrictDatabase>,
    /// Optional `[weather]` table.
    weather: Option<StrictWeather>,
    /// Optional `[automation]` table.
    automation: Option<StrictAutomation>,
}

// =============================================================================
/// Optional `[app]` section accepted when validating older `config.toml` files.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct StrictLegacyApp {
    /// Application display name.
    name: Option<String>,
    /// Author name.
    author: Option<String>,
    /// Package description.
    description: Option<String>,
    /// Suite title.
    suite_title: Option<String>,
}

// =============================================================================
/// Strict `[debug]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictDebug {
    /// Raw severity string parsed via [`LogLevel::parse_strict`].
    level: Option<String>,
    /// Optional ANSI color toggle.
    console_colors: Option<bool>,
}

// =============================================================================
/// Strict `[server]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictServer {
    /// Bind host or IP literal.
    host: Option<String>,
    /// TCP listen port.
    port: Option<u16>,
    /// External base URL.
    base_url: Option<String>,
}

// =============================================================================
/// Strict `[api]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictApi {
    /// API secret key.
    api_key: Option<String>,
    /// Trusted reverse proxy IPs or CIDRs.
    trusted_proxies: Option<Vec<String>>,
}

// =============================================================================
/// Strict `[rate_limit]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictRateLimit {
    /// Rate limiter activation flag.
    enabled: Option<bool>,
    /// Auth lockout window in seconds.
    window: Option<u64>,
    /// Max failed logins per window.
    max_login_attempts: Option<u32>,
    /// Max requests per minute per IP.
    max_requests_per_minute_per_ip: Option<u32>,
}

// =============================================================================
/// Strict `[fritzbox]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictFritzbox {
    /// FritzBox hostname or IP.
    host: Option<String>,
    /// FritzBox username.
    username: Option<String>,
    /// FritzBox password.
    password: Option<String>,
    /// Polling interval in seconds.
    poll_interval: Option<u64>,
}

// =============================================================================
/// Strict `[daemon]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictDaemon {
    /// Background daemon enablement toggle.
    enabled: Option<bool>,
}

// =============================================================================
/// Strict `[database]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictDatabase {
    /// Relative or absolute SQLite database path.
    db_path: Option<String>,
}

// =============================================================================
/// Strict `[weather]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictWeather {
    /// Home latitude.
    latitude: Option<f64>,
    /// Home longitude.
    longitude: Option<f64>,
    /// Home IANA timezone or `"auto"`.
    timezone: Option<String>,
    /// Update interval in seconds.
    update_interval: Option<u64>,
    /// Forecast days (`1`..=`16`).
    forecast_days: Option<u8>,
}

// =============================================================================
/// Strict `[automation]` section schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictAutomation {
    /// RulesEngine evaluation interval in seconds.
    interval: Option<u64>,
    /// Global vacation mode flag.
    vacation_mode: Option<bool>,
}

// =============================================================================
/// Strictly validates a TOML configuration file (`cowhome validate [--file <path>]`).
///
/// # Errors
/// Returns [`AppError::Auth`] if `0750`/`0640` permissions deny read access, or
/// [`AppError::Validation`] on missing files, symlinks, unknown keys, or invalid values.
pub fn validate_config_file(explicit_file: Option<&Path>) -> AppResult<(AppConfig, PathBuf)> {
    let path = resolve_config_path(explicit_file);
    match probe_path_presence(&path) {
        PathPresence::Missing => {
            return Err(AppError::Validation(format!(
                "Configuration file '{}' not found",
                path.display()
            )));
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
    let cfg = parse_strict_config_toml(&raw_text, &path.display().to_string())?;
    Ok((cfg, path))
}

// =============================================================================
/// Strictly parses and validates a TOML string (`deny_unknown_fields`) into [`AppConfig`].
///
/// # Errors
/// Returns [`AppError::Validation`] if unknown keys, syntax errors, or invalid bounds occur.
pub fn parse_strict_config_toml(raw_text: &str, source_label: &str) -> AppResult<AppConfig> {
    let strict: StrictConfigSchema = toml::from_str(raw_text).map_err(|err| {
        AppError::Validation(format!(
            "TOML schema/syntax error in '{source_label}': {err}"
        ))
    })?;
    let cfg = convert_strict_to_config(strict)?;
    cfg.validate()?;
    Ok(cfg)
}

// =============================================================================
/// Maps a parsed [`StrictConfigSchema`] into an [`AppConfig`] ready for domain validation.
///
/// # Errors
/// Returns [`AppError::Validation`] if `debug.level` is not a valid [`LogLevel`].
fn convert_strict_to_config(strict: StrictConfigSchema) -> AppResult<AppConfig> {
    let mut cfg = AppConfig::default();
    if let Some(dbg) = strict.debug {
        if let Some(lvl) = dbg.level {
            cfg.debug.level = LogLevel::parse_strict(&lvl)?;
        }
        if let Some(colors) = dbg.console_colors {
            cfg.debug.console_colors = colors;
        }
    }
    if let Some(srv) = strict.server {
        apply_strict_server(&mut cfg.server, srv);
    }
    if let Some(api) = strict.api {
        apply_strict_api(&mut cfg.api, api);
    }
    if let Some(rl) = strict.rate_limit {
        apply_strict_rate_limit(&mut cfg.rate_limit, rl);
    }
    apply_strict_domain_sections(
        &mut cfg,
        strict.fritzbox,
        strict.daemon,
        strict.database,
        strict.weather,
        strict.automation,
    );
    Ok(cfg)
}

// =============================================================================
/// Applies optional strict `[server]` fields onto `target`.
fn apply_strict_server(target: &mut ServerConfig, src: StrictServer) {
    if let Some(v) = src.host {
        target.host = v;
    }
    if let Some(v) = src.port {
        target.port = v;
    }
    if let Some(v) = src.base_url {
        target.base_url = v;
    }
}

// =============================================================================
/// Applies optional strict `[api]` fields onto `target`.
fn apply_strict_api(target: &mut ApiConfig, src: StrictApi) {
    if let Some(v) = src.api_key {
        target.api_key = v;
    }
    if let Some(v) = src.trusted_proxies {
        target.trusted_proxies = v;
    }
}

// =============================================================================
/// Applies optional strict `[rate_limit]` fields onto `target`.
fn apply_strict_rate_limit(target: &mut RateLimitConfig, src: StrictRateLimit) {
    if let Some(v) = src.enabled {
        target.enabled = v;
    }
    if let Some(v) = src.window {
        target.window = v;
    }
    if let Some(v) = src.max_login_attempts {
        target.max_login_attempts = v;
    }
    if let Some(v) = src.max_requests_per_minute_per_ip {
        target.max_requests_per_minute_per_ip = v;
    }
}

// =============================================================================
/// Applies optional `[fritzbox]`, `[daemon]`, `[database]`, `[weather]`, and `[automation]` fields.
fn apply_strict_domain_sections(
    cfg: &mut AppConfig,
    fb: Option<StrictFritzbox>,
    daemon: Option<StrictDaemon>,
    db: Option<StrictDatabase>,
    weather: Option<StrictWeather>,
    auto: Option<StrictAutomation>,
) {
    if let Some(f) = fb {
        apply_strict_fritzbox(&mut cfg.fritzbox, f);
    }
    if let Some(enabled) = daemon.and_then(|d| d.enabled) {
        cfg.daemon.enabled = enabled;
    }
    if let Some(db_path) = db.and_then(|d| d.db_path) {
        cfg.database.db_path = db_path;
    }
    if let Some(w) = weather {
        apply_strict_weather(&mut cfg.weather, w);
    }
    if let Some(a) = auto {
        apply_strict_automation(&mut cfg.automation, a);
    }
}

// =============================================================================
/// Applies optional strict `[fritzbox]` fields onto `target`.
fn apply_strict_fritzbox(target: &mut FritzboxConfig, src: StrictFritzbox) {
    if let Some(v) = src.host {
        target.host = v;
    }
    if let Some(v) = src.username {
        target.username = v;
    }
    if let Some(v) = src.password {
        target.password = v;
    }
    if let Some(v) = src.poll_interval {
        target.poll_interval = v;
    }
}

// =============================================================================
/// Applies optional strict `[weather]` fields onto `target`.
fn apply_strict_weather(target: &mut WeatherConfig, src: StrictWeather) {
    if let Some(v) = src.latitude {
        target.latitude = v;
    }
    if let Some(v) = src.longitude {
        target.longitude = v;
    }
    if let Some(v) = src.timezone {
        target.timezone = v;
    }
    if let Some(v) = src.update_interval {
        target.update_interval = v;
    }
    if let Some(v) = src.forecast_days {
        target.forecast_days = v;
    }
}

// =============================================================================
/// Applies optional strict `[automation]` fields onto `target`.
fn apply_strict_automation(target: &mut AutomationConfig, src: StrictAutomation) {
    if let Some(v) = src.interval {
        target.interval = v;
    }
    if let Some(v) = src.vacation_mode {
        target.vacation_mode = v;
    }
}
