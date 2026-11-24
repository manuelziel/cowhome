//! Section-level interactive editors for `cowhome config`.
//!
//! Groups configuration prompts into focused 2–5 field dialogs (`Server`,
//! `FritzBox`, `Weather`, and `Limits` with sub-selector) per
//! `_project-resources/docs/design_guidelines.md` Progressive Disclosure rules.

use crate::cli::config_prompts::{
    prompt_api_key, prompt_bool_or_cancel, prompt_checked_host, prompt_checked_path,
    prompt_checked_timezone, prompt_checked_url, prompt_lat_lon, prompt_masked_secret,
    prompt_or_cancel, prompt_positive_int, prompt_trusted_proxies, prompt_weather_horizon,
};
use crate::cli::input::prompt_action_from_list;
use crate::cli::output::ConsolePrinter;
use crate::core::config::AppConfig;
use crate::core::error::AppResult;
use crate::core::logger::LogLevel;

// =============================================================================
/// Typed sub-sections inside `Limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemSubsection {
    /// Edit sliding-window HTTP rate limiter bounds (`[rate_limit]`).
    RateLimiting,
    /// Edit rules evaluation interval, vacation mode, and daemon flag.
    AutomationAndDaemon,
    /// Edit SQLite file path (`[database]`).
    DatabasePath,
    /// Edit log verbosity and ANSI console colors (`[debug]`).
    LoggingAndColors,
}

/// Sub-sections inside `Limits` using concise M3 noun labels (`design_guidelines.md` § 8).
const SYSTEM_SUBSECTIONS: &[(SystemSubsection, &str)] = &[
    (SystemSubsection::RateLimiting, "Rate Limiting"),
    (SystemSubsection::AutomationAndDaemon, "Automation"),
    (SystemSubsection::DatabasePath, "Database"),
    (SystemSubsection::LoggingAndColors, "Logging"),
];

// =============================================================================
/// Edits `[server]` (`host`, `port`, `base_url`) and `[api]` (`api_key`, `trusted_proxies`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_server_and_api_section(
    working: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<()> {
    printer.print_subsection("Server");
    let Some((host, port, base_url)) = prompt_server_fields(working, printer)? else {
        return Ok(());
    };
    let Some(api_key) = prompt_api_key(&working.api.api_key, printer)? else {
        return Ok(());
    };
    let Some(proxies) = prompt_trusted_proxies(&working.api.trusted_proxies, printer)? else {
        return Ok(());
    };
    let changed = host != working.server.host
        || port != working.server.port
        || base_url != working.server.base_url
        || api_key != working.api.api_key
        || proxies != working.api.trusted_proxies;
    working.server.host = host;
    working.server.port = port;
    working.server.base_url = base_url;
    working.api.api_key = api_key;
    working.api.trusted_proxies = proxies;
    if changed {
        printer.print_success("Updated Server settings.");
    }
    Ok(())
}

// =============================================================================
/// Prompts and validates `server.host`, `server.port`, and `server.base_url`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn prompt_server_fields(
    working: &AppConfig,
    printer: ConsolePrinter,
) -> AppResult<Option<(String, u16, String)>> {
    let Some(host) = prompt_checked_host("Server bind address", &working.server.host, printer)?
    else {
        return Ok(None);
    };
    let Some(port) = prompt_positive_int("Server port", working.server.port, printer)? else {
        return Ok(None);
    };
    let Some(base_url) =
        prompt_checked_url("External base URL", &working.server.base_url, printer)?
    else {
        return Ok(None);
    };
    Ok(Some((host, port, base_url)))
}

// =============================================================================
/// Edits `[fritzbox]` (`host`, `username`, `password`, `poll_interval`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_fritzbox_section(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    printer.print_subsection("FritzBox");
    let Some(host) = prompt_checked_host("FritzBox host", &working.fritzbox.host, printer)? else {
        return Ok(());
    };
    let Some(username) = prompt_or_cancel("FritzBox username", Some(&working.fritzbox.username))?
    else {
        return Ok(());
    };
    let Some(password) = prompt_masked_secret("FritzBox password", &working.fritzbox.password)?
    else {
        return Ok(());
    };
    let Some(poll_interval) = prompt_positive_int(
        "Poll interval in seconds",
        working.fritzbox.poll_interval,
        printer,
    )?
    else {
        return Ok(());
    };
    let changed = host != working.fritzbox.host
        || username != working.fritzbox.username
        || password != working.fritzbox.password
        || poll_interval != working.fritzbox.poll_interval;
    working.fritzbox.host = host;
    working.fritzbox.username = username;
    working.fritzbox.password = password;
    working.fritzbox.poll_interval = poll_interval;
    if changed {
        printer.print_success("Updated FritzBox settings.");
    }
    Ok(())
}

// =============================================================================
/// Edits `[weather]` (`latitude`, `longitude`, `timezone`, `update_interval`, `forecast_days`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_weather_section(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    printer.print_subsection("Weather");
    let Some((lat, lon)) =
        prompt_lat_lon(working.weather.latitude, working.weather.longitude, printer)?
    else {
        return Ok(());
    };
    let Some(tz) = prompt_checked_timezone(&working.weather.timezone, printer)? else {
        return Ok(());
    };
    let Some((upd, days)) = prompt_weather_horizon(
        working.weather.update_interval,
        working.weather.forecast_days,
        printer,
    )?
    else {
        return Ok(());
    };
    let changed = (lat - working.weather.latitude).abs() > f64::EPSILON
        || (lon - working.weather.longitude).abs() > f64::EPSILON
        || tz != working.weather.timezone
        || upd != working.weather.update_interval
        || days != working.weather.forecast_days;
    working.weather.latitude = lat;
    working.weather.longitude = lon;
    working.weather.timezone = tz;
    working.weather.update_interval = upd;
    working.weather.forecast_days = days;
    if changed {
        printer.print_success("Updated Weather settings.");
    }
    Ok(())
}

// =============================================================================
/// Opens a focused sub-selector for `Rate Limiting`, `Automation`, `Database`, or `Logging`;
/// returns `false` when `0) Cancel` is chosen so the caller skips `wait_for_enter`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_system_and_limits_section(
    working: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    printer.print_subsection("Limits");
    let Some(selected) = prompt_action_from_list(SYSTEM_SUBSECTIONS, "Select section", printer)?
    else {
        return Ok(false);
    };
    eprintln!();
    match selected {
        SystemSubsection::RateLimiting => edit_rate_limit_fields(working, printer)?,
        SystemSubsection::AutomationAndDaemon => {
            edit_automation_and_daemon_fields(working, printer)?;
        }
        SystemSubsection::DatabasePath => edit_database_path_field(working, printer)?,
        SystemSubsection::LoggingAndColors => edit_debug_section(working, printer)?,
    }
    Ok(true)
}

// =============================================================================
/// Edits `working.rate_limit` (`enabled`, `window`, `max_login_attempts`, `rpm`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_rate_limit_fields(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    let before = working.rate_limit.clone();
    let Some(rl_en) = prompt_bool_or_cancel("Enable rate limiting?", before.enabled)? else {
        return Ok(());
    };
    let Some(win) = prompt_positive_int("Lockout window in seconds", before.window, printer)?
    else {
        return Ok(());
    };
    let Some(logins) = prompt_positive_int(
        "Max failed logins per window",
        before.max_login_attempts,
        printer,
    )?
    else {
        return Ok(());
    };
    let Some(rpm) = prompt_positive_int(
        "Max requests per minute per IP",
        before.max_requests_per_minute_per_ip,
        printer,
    )?
    else {
        return Ok(());
    };
    working.rate_limit.enabled = rl_en;
    working.rate_limit.window = win;
    working.rate_limit.max_login_attempts = logins;
    working.rate_limit.max_requests_per_minute_per_ip = rpm;
    if working.rate_limit != before {
        printer.print_success("Updated Rate Limiting settings.");
    }
    Ok(())
}

// =============================================================================
/// Edits `working.automation` (`interval`, `vacation_mode`) and `working.daemon.enabled`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_automation_and_daemon_fields(
    working: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let Some(auto_int) = prompt_positive_int(
        "Rules evaluation interval in seconds",
        working.automation.interval,
        printer,
    )?
    else {
        return Ok(());
    };
    let Some(vac) = prompt_bool_or_cancel(
        "Enable global vacation mode?",
        working.automation.vacation_mode,
    )?
    else {
        return Ok(());
    };
    let Some(daemon_en) =
        prompt_bool_or_cancel("Enable background workers?", working.daemon.enabled)?
    else {
        return Ok(());
    };
    let changed = auto_int != working.automation.interval
        || vac != working.automation.vacation_mode
        || daemon_en != working.daemon.enabled;
    working.automation.interval = auto_int;
    working.automation.vacation_mode = vac;
    working.daemon.enabled = daemon_en;
    if changed {
        printer.print_success("Updated Automation settings.");
    }
    Ok(())
}

// =============================================================================
/// Edits `working.database.db_path` with traversal validation.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_database_path_field(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    let Some(db_path) = prompt_checked_path(
        "SQLite database path",
        &working.database.db_path,
        "database.db_path",
        printer,
    )?
    else {
        return Ok(());
    };
    let changed = db_path != working.database.db_path;
    working.database.db_path = db_path;
    if changed {
        printer.print_success("Updated Database settings.");
    }
    Ok(())
}

// =============================================================================
/// Edits `[debug]` (`level` and `console_colors`) with retry on invalid level names.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_debug_section(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    let parsed_lvl = loop {
        let Some(raw_lvl) = prompt_or_cancel("Log level", Some(working.debug.level.as_str()))?
        else {
            return Ok(());
        };
        match LogLevel::parse_strict(&raw_lvl) {
            Ok(lvl) => break lvl,
            Err(err) => printer.print_error(&err.to_string()),
        }
    };
    let Some(colors) = prompt_bool_or_cancel(
        "Enable colored console output?",
        working.debug.console_colors,
    )?
    else {
        return Ok(());
    };
    let changed = parsed_lvl != working.debug.level || colors != working.debug.console_colors;
    working.debug.level = parsed_lvl;
    working.debug.console_colors = colors;
    if changed {
        printer.print_success("Updated Logging settings.");
    }
    Ok(())
}
