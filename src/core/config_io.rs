//! Atomic configuration persistence (`save_config`) and deterministic TOML formatting.
//!
//! Separated from [`crate::core::config`] and [`crate::core::config_schema`] to maintain
//! strict single-responsibility module boundaries (`< 250` lines).

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::config::AppConfig;
use crate::core::constants::app_meta::NAME;
use crate::core::constants::fhs_paths::CONFIG_DIR;
use crate::core::constants::service_defaults::{
    PRIVATE_FILE_MODE, SERVICE_GROUP, SUDO_TIMEOUT_SECS,
};
use crate::core::error::{AppError, AppResult};
use crate::core::process::run_command_inherited;
use crate::core::security::{
    apply_ownership_if_root, atomic_write_file, ensure_not_symlink, is_root,
};

// =============================================================================
/// Validates `config` and writes it atomically (elevating via `sudo` for `/etc` when non-root).
///
/// # Errors
/// Returns [`AppError`] on validation, symlink, `sudo`, or filesystem write failure.
pub fn save_config(config: &AppConfig, target_path: &Path) -> AppResult<()> {
    config.validate()?;
    ensure_not_symlink(target_path)?;
    let serialized = format_config_toml(config);
    let mode = config.target_file_mode(target_path);

    let needs_sudo =
        !is_root() && (target_path.starts_with("/etc/") || target_path.starts_with(CONFIG_DIR));
    if needs_sudo {
        return save_config_via_sudo(serialized.as_bytes(), target_path, mode);
    }
    atomic_write_file(target_path, serialized.as_bytes(), mode)?;
    apply_ownership_if_root(target_path, "root", SERVICE_GROUP)
}

// =============================================================================
/// Stages `contents` in a `0600` tempfile and installs into `/etc/...` via `sudo install`.
///
/// # Errors
/// Returns [`AppError`] if staging or `sudo install` fails.
fn save_config_via_sudo(contents: &[u8], target_path: &Path, mode: u32) -> AppResult<()> {
    let target_str = target_path.to_str().ok_or_else(|| {
        AppError::Validation(format!("Non-UTF-8 config path '{}'", target_path.display()))
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp_path =
        std::env::temp_dir().join(format!(".{NAME}-cfg.{}.{nanos}.tmp", std::process::id()));
    let temp_str = temp_path
        .to_str()
        .ok_or_else(|| AppError::Config("Temporary directory path is not valid UTF-8".into()))?;

    atomic_write_file(&temp_path, contents, PRIVATE_FILE_MODE)?;
    let res = run_sudo_config_install(temp_str, target_path, target_str, mode);
    if let Err(_cleanup_err) = fs::remove_file(&temp_path) {
        // Staging file cleanup is best-effort after `sudo install` completes.
    }
    res
}

// =============================================================================
/// Ensures the target parent exists (`! -L`) and runs `sudo install` with octal `mode`.
///
/// # Errors
/// Returns [`AppError`] if `target_str` is a symlink or `sudo install` exits non-zero.
fn run_sudo_config_install(
    temp_str: &str,
    target_path: &Path,
    target_str: &str,
    mode: u32,
) -> AppResult<()> {
    let timeout = Duration::from_secs(SUDO_TIMEOUT_SECS);
    ensure_sudo_parent_dir(target_path, timeout)?;
    if !run_command_inherited("sudo", &["--", "test", "!", "-L", target_str], timeout)? {
        return Err(AppError::Security(format!(
            "Symbolic link rejected (CWE-59): '{target_str}'"
        )));
    }
    let mode_octal = format!("{mode:04o}");
    let args = [
        "--",
        "install",
        "-o",
        "root",
        "-g",
        SERVICE_GROUP,
        "-m",
        &mode_octal,
        "--",
        temp_str,
        target_str,
    ];
    if !run_command_inherited("sudo", &args, timeout)? {
        return Err(AppError::Config(format!(
            "Failed installing configuration to '{target_str}' via sudo"
        )));
    }
    Ok(())
}

// =============================================================================
/// Creates `target_path`'s parent directory under `sudo mkdir -p` when needed.
///
/// # Errors
/// Returns [`AppError::Config`] if `sudo mkdir -p` fails.
fn ensure_sudo_parent_dir(target_path: &Path, timeout: Duration) -> AppResult<()> {
    if let Some(parent_str) = target_path.parent().and_then(Path::to_str) {
        if !parent_str.is_empty()
            && !run_command_inherited("sudo", &["--", "mkdir", "-p", "--", parent_str], timeout)?
        {
            return Err(AppError::Config(format!(
                "Failed creating directory '{parent_str}' via sudo"
            )));
        }
    }
    Ok(())
}

// =============================================================================
/// Renders `config` into clean, commented `TOML v1.0` matching `assets/config.default.toml`.
#[must_use]
pub fn format_config_toml(config: &AppConfig) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(
        "# =========================\n# CowHome Configuration\n# =========================\n\n",
    );
    append_server_and_security_toml(&mut out, config);
    append_domain_sections_toml(&mut out, config);
    out
}

// =============================================================================
/// Appends `[debug]`, `[server]`, `[api]`, and `[rate_limit]` TOML tables to `out`.
fn append_server_and_security_toml(out: &mut String, config: &AppConfig) {
    let proxies = toml::Value::from(config.api.trusted_proxies.clone()).to_string();
    out.push_str(&format!(
        "# ----------- DEBUGGING -----------\n[debug]\nlevel = {}\nconsole_colors = {}\n\n",
        toml_quote(config.debug.level.as_str()),
        config.debug.console_colors
    ));
    out.push_str(&format!(
        "# ----------- SERVER (HTTP REST API) -----------\n[server]\nhost = {}\nport = {}\nbase_url = {}\n\n",
        toml_quote(&config.server.host),
        config.server.port,
        toml_quote(&config.server.base_url)
    ));
    out.push_str(&format!(
        "# ----------- API SECURITY -----------\n[api]\napi_key = {}\ntrusted_proxies = {proxies}\n\n",
        toml_quote(&config.api.api_key)
    ));
    out.push_str(&format!(
        "# ----------- RATE LIMITING -----------\n[rate_limit]\nenabled = {}\nwindow = {}\nmax_login_attempts = {}\nmax_requests_per_minute_per_ip = {}\n\n",
        config.rate_limit.enabled,
        config.rate_limit.window,
        config.rate_limit.max_login_attempts,
        config.rate_limit.max_requests_per_minute_per_ip
    ));
}

// =============================================================================
/// Appends `[fritzbox]`, `[daemon]`, `[database]`, `[weather]`, and `[automation]` tables.
fn append_domain_sections_toml(out: &mut String, config: &AppConfig) {
    out.push_str(&format!(
        "# ----------- FRITZBOX -----------\n[fritzbox]\nhost = {}\nusername = {}\npassword = {}\npoll_interval = {}\n\n",
        toml_quote(&config.fritzbox.host),
        toml_quote(&config.fritzbox.username),
        toml_quote(&config.fritzbox.password),
        config.fritzbox.poll_interval
    ));
    out.push_str(&format!(
        "# ----------- DAEMON CONFIGURATION -----------\n[daemon]\nenabled = {}\n\n",
        config.daemon.enabled
    ));
    out.push_str(&format!(
        "# ----------- DATABASE -----------\n[database]\ndb_path = {}\n\n",
        toml_quote(&config.database.db_path)
    ));
    out.push_str(&format!(
        "# ----------- WEATHER (Open-Meteo) -----------\n[weather]\nlatitude = {}\nlongitude = {}\ntimezone = {}\nupdate_interval = {}\nforecast_days = {}\n\n",
        config.weather.latitude,
        config.weather.longitude,
        toml_quote(&config.weather.timezone),
        config.weather.update_interval,
        config.weather.forecast_days
    ));
    out.push_str(&format!(
        "# ----------- AUTOMATION -----------\n[automation]\ninterval = {}\nvacation_mode = {}\n",
        config.automation.interval, config.automation.vacation_mode
    ));
}

// =============================================================================
/// Escapes a string literal strictly according to TOML v1.0 quoting rules.
fn toml_quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}
