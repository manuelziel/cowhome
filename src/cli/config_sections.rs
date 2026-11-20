//! Section-level interactive editors for `cowhome config`.
//!
//! Groups configuration prompts into focused 2–5 field dialogs (`Server & API`,
//! `FritzBox`, `Weather & Timezone`, and `System, Limits & Logging` with sub-selector)
//! per `_project-resources/docs/design_guidelines.md` Progressive Disclosure rules.

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
/// Typed sub-sections inside `System, Limits & Logging`.
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

/// Sub-sections inside `System, Limits & Logging` so admins only edit what they need.
const SYSTEM_SUBSECTIONS: &[(SystemSubsection, &str)] = &[
    (SystemSubsection::RateLimiting, "Rate Limiting"),
    (SystemSubsection::AutomationAndDaemon, "Automation & Daemon"),
    (SystemSubsection::DatabasePath, "Database Path"),
    (SystemSubsection::LoggingAndColors, "Logging & Colors"),
];

// =============================================================================
/// Edits `[server]` (`host`, `port`, `base_url`) and `[api]` (`api_key`, `trusted_proxies`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_server_and_api_section(
    draft: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    printer.print_subsection("Server & API");
    let Some((host, port, base_url)) = prompt_server_fields(draft, printer)? else {
        return Ok(false);
    };
    let Some(api_key) = prompt_api_key(&draft.api.api_key, printer)? else {
        return Ok(false);
    };
    let Some(proxies) = prompt_trusted_proxies(&draft.api.trusted_proxies, printer)? else {
        return Ok(false);
    };
    let changed = host != draft.server.host
        || port != draft.server.port
        || base_url != draft.server.base_url
        || api_key != draft.api.api_key
        || proxies != draft.api.trusted_proxies;
    draft.server.host = host;
    draft.server.port = port;
    draft.server.base_url = base_url;
    draft.api.api_key = api_key;
    draft.api.trusted_proxies = proxies;
    if changed {
        printer.print_success("Updated Server & API settings.");
    }
    Ok(changed)
}

// =============================================================================
/// Prompts and validates `server.host`, `server.port`, and `server.base_url`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn prompt_server_fields(
    draft: &AppConfig,
    printer: ConsolePrinter,
) -> AppResult<Option<(String, u16, String)>> {
    let Some(host) = prompt_checked_host("Server bind address", &draft.server.host, printer)?
    else {
        return Ok(None);
    };
    let Some(port) = prompt_positive_int("Server port", draft.server.port, printer)? else {
        return Ok(None);
    };
    let Some(base_url) = prompt_checked_url("External base URL", &draft.server.base_url, printer)?
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
pub fn edit_fritzbox_section(draft: &mut AppConfig, printer: ConsolePrinter) -> AppResult<bool> {
    printer.print_subsection("FritzBox");
    let Some(host) = prompt_checked_host("FritzBox host", &draft.fritzbox.host, printer)? else {
        return Ok(false);
    };
    let Some(username) = prompt_or_cancel("FritzBox username", Some(&draft.fritzbox.username))?
    else {
        return Ok(false);
    };
    let Some(password) = prompt_masked_secret("FritzBox password", &draft.fritzbox.password)?
    else {
        return Ok(false);
    };
    let Some(poll_interval) = prompt_positive_int(
        "Poll interval in seconds",
        draft.fritzbox.poll_interval,
        printer,
    )?
    else {
        return Ok(false);
    };
    let changed = host != draft.fritzbox.host
        || username != draft.fritzbox.username
        || password != draft.fritzbox.password
        || poll_interval != draft.fritzbox.poll_interval;
    draft.fritzbox.host = host;
    draft.fritzbox.username = username;
    draft.fritzbox.password = password;
    draft.fritzbox.poll_interval = poll_interval;
    if changed {
        printer.print_success("Updated FritzBox settings.");
    }
    Ok(changed)
}

// =============================================================================
/// Edits `[weather]` (`latitude`, `longitude`, `timezone`, `update_interval`, `forecast_days`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_weather_section(draft: &mut AppConfig, printer: ConsolePrinter) -> AppResult<bool> {
    printer.print_subsection("Weather & Timezone");
    let Some((lat, lon)) =
        prompt_lat_lon(draft.weather.latitude, draft.weather.longitude, printer)?
    else {
        return Ok(false);
    };
    let Some(tz) = prompt_checked_timezone(&draft.weather.timezone, printer)? else {
        return Ok(false);
    };
    let Some((upd, days)) = prompt_weather_horizon(
        draft.weather.update_interval,
        draft.weather.forecast_days,
        printer,
    )?
    else {
        return Ok(false);
    };
    let changed = (lat - draft.weather.latitude).abs() > f64::EPSILON
        || (lon - draft.weather.longitude).abs() > f64::EPSILON
        || tz != draft.weather.timezone
        || upd != draft.weather.update_interval
        || days != draft.weather.forecast_days;
    draft.weather.latitude = lat;
    draft.weather.longitude = lon;
    draft.weather.timezone = tz;
    draft.weather.update_interval = upd;
    draft.weather.forecast_days = days;
    if changed {
        printer.print_success("Updated Weather & Timezone settings.");
    }
    Ok(changed)
}

// =============================================================================
/// Opens a focused sub-selector for `Rate Limiting`, `Automation`, `Database`, or `Logging`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_system_and_limits_section(
    draft: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    printer.print_subsection("System, Limits & Logging");
    let Some(selected) = prompt_action_from_list(SYSTEM_SUBSECTIONS, "Select section", printer)?
    else {
        return Ok(false);
    };
    eprintln!();
    match selected {
        SystemSubsection::RateLimiting => edit_rate_limit_fields(draft, printer),
        SystemSubsection::AutomationAndDaemon => edit_automation_and_daemon_fields(draft, printer),
        SystemSubsection::DatabasePath => edit_database_path_field(draft, printer),
        SystemSubsection::LoggingAndColors => edit_debug_section(draft, printer),
    }
}

// =============================================================================
/// Edits `draft.rate_limit` (`enabled`, `window`, `max_login_attempts`, `rpm`).
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_rate_limit_fields(draft: &mut AppConfig, printer: ConsolePrinter) -> AppResult<bool> {
    let before = draft.rate_limit.clone();
    let Some(rl_en) = prompt_bool_or_cancel("Enable rate limiting?", before.enabled)? else {
        return Ok(false);
    };
    let Some(win) = prompt_positive_int("Lockout window in seconds", before.window, printer)?
    else {
        return Ok(false);
    };
    let Some(logins) = prompt_positive_int(
        "Max failed logins per window",
        before.max_login_attempts,
        printer,
    )?
    else {
        return Ok(false);
    };
    let Some(rpm) = prompt_positive_int(
        "Max requests per minute per IP",
        before.max_requests_per_minute_per_ip,
        printer,
    )?
    else {
        return Ok(false);
    };
    draft.rate_limit.enabled = rl_en;
    draft.rate_limit.window = win;
    draft.rate_limit.max_login_attempts = logins;
    draft.rate_limit.max_requests_per_minute_per_ip = rpm;
    let changed = draft.rate_limit != before;
    if changed {
        printer.print_success("Updated Rate Limiting settings.");
    }
    Ok(changed)
}

// =============================================================================
/// Edits `draft.automation` (`interval`, `vacation_mode`) and `draft.daemon.enabled`.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_automation_and_daemon_fields(
    draft: &mut AppConfig,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    let Some(auto_int) = prompt_positive_int(
        "Rules evaluation interval in seconds",
        draft.automation.interval,
        printer,
    )?
    else {
        return Ok(false);
    };
    let Some(vac) = prompt_bool_or_cancel(
        "Enable global vacation mode?",
        draft.automation.vacation_mode,
    )?
    else {
        return Ok(false);
    };
    let Some(daemon_en) =
        prompt_bool_or_cancel("Enable background workers?", draft.daemon.enabled)?
    else {
        return Ok(false);
    };
    let changed = auto_int != draft.automation.interval
        || vac != draft.automation.vacation_mode
        || daemon_en != draft.daemon.enabled;
    draft.automation.interval = auto_int;
    draft.automation.vacation_mode = vac;
    draft.daemon.enabled = daemon_en;
    if changed {
        printer.print_success("Updated Automation & Daemon settings.");
    }
    Ok(changed)
}

// =============================================================================
/// Edits `draft.database.db_path` with traversal validation.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
fn edit_database_path_field(draft: &mut AppConfig, printer: ConsolePrinter) -> AppResult<bool> {
    let Some(db_path) = prompt_checked_path(
        "SQLite database path",
        &draft.database.db_path,
        "database.db_path",
        printer,
    )?
    else {
        return Ok(false);
    };
    let changed = db_path != draft.database.db_path;
    draft.database.db_path = db_path;
    if changed {
        printer.print_success("Updated Database path.");
    }
    Ok(changed)
}

// =============================================================================
/// Edits `[debug]` (`level` and `console_colors`) with retry on invalid level names.
///
/// # Errors
/// Returns [`crate::core::error::AppError::Io`] on unrecoverable terminal stream errors.
pub fn edit_debug_section(draft: &mut AppConfig, printer: ConsolePrinter) -> AppResult<bool> {
    let parsed_lvl = loop {
        let Some(raw_lvl) = prompt_or_cancel("Log level", Some(draft.debug.level.as_str()))? else {
            return Ok(false);
        };
        match LogLevel::parse_strict(&raw_lvl) {
            Ok(lvl) => break lvl,
            Err(err) => printer.print_error(&err.to_string()),
        }
    };
    let Some(colors) =
        prompt_bool_or_cancel("Enable colored console output?", draft.debug.console_colors)?
    else {
        return Ok(false);
    };
    let changed = parsed_lvl != draft.debug.level || colors != draft.debug.console_colors;
    draft.debug.level = parsed_lvl;
    draft.debug.console_colors = colors;
    if changed {
        printer.print_success("Updated Logging & Colors.");
    }
    Ok(changed)
}
