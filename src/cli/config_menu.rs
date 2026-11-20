//! Interactive TUI configuration & sysadmin manager (`cowhome config`).
//!
//! Renders dynamically numbered, scannable menu options (zero hardcoded indices or
//! technical bracket noise), grouped summaries, non-destructive validation on save,
//! FritzBox connectivity testing, break-glass automation shutdown, and Backup & Restore.

use std::path::Path;

use crate::cli::config_sections::{
    edit_fritzbox_section, edit_server_and_api_section, edit_system_and_limits_section,
    edit_weather_section,
};
use crate::cli::emergency_disable::run_emergency_disable_automations;
use crate::cli::fritz_test::run_fritz_connection_test;
use crate::cli::import_export::{run_interactive_export, run_interactive_import};
use crate::cli::input::{
    prompt_action_from_list, prompt_confirm, read_menu_choice_once, resolve_menu_action,
    stdin_is_interactive, wait_for_enter,
};
use crate::cli::output::ConsolePrinter;
use crate::cli::status_view::{format_api_key_badge, show_runtime_status};
use crate::core::config::{save_config, AppConfig};
use crate::core::constants::app_meta::NAME;
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::validation::mask_secret;

// =============================================================================
/// Typed actions exposed in the dynamically numbered `cowhome config` menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMenuAction {
    /// Edit HTTP bind address, port, base URL, API key, and trusted proxies.
    ServerAndApi,
    /// Edit FritzBox host, credentials, and polling interval.
    Fritzbox,
    /// Edit home coordinates, IANA timezone, and forecast horizon.
    Weather,
    /// Edit rate limits, automation/daemon intervals, DB path, or logging.
    SystemAndLimits,
    /// Run live `TR-064` & `AHA` connection test with current draft settings.
    TestFritzbox,
    /// Break-glass shutdown of all active automations across scopes.
    EmergencyDisable,
    /// Export or import SQLite backups (`YAML` / `JSON` / `TOML`).
    BackupAndRestore,
    /// Inspect current service, storage, and live device status.
    ViewStatus,
}

/// Ordered `(action, label)` table driving both rendering and index lookup.
const CONFIG_MENU_ITEMS: &[(ConfigMenuAction, &str)] = &[
    (ConfigMenuAction::ServerAndApi, "Server & API"),
    (ConfigMenuAction::Fritzbox, "FritzBox"),
    (ConfigMenuAction::Weather, "Weather & Timezone"),
    (
        ConfigMenuAction::SystemAndLimits,
        "System, Limits & Logging",
    ),
    (ConfigMenuAction::TestFritzbox, "Test FritzBox Connection"),
    (
        ConfigMenuAction::EmergencyDisable,
        "Emergency: Disable Automations",
    ),
    (ConfigMenuAction::BackupAndRestore, "Backup & Restore"),
    (ConfigMenuAction::ViewStatus, "Status"),
];

// =============================================================================
/// Typed actions inside the `Backup & Restore` sub-selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupMenuAction {
    /// Export SQLite backup or configuration file.
    Export,
    /// Import SQLite backup or configuration file.
    Import,
}

/// Ordered `(action, label)` slice for `Backup & Restore`.
const BACKUP_MENU_ITEMS: &[(BackupMenuAction, &str)] = &[
    (BackupMenuAction::Export, "Export Backup"),
    (BackupMenuAction::Import, "Import Backup"),
];

// =============================================================================
/// Runs the interactive configuration & sysadmin menu loop for `config_path`.
///
/// # Errors
/// Returns [`AppError`] on non-interactive `stdin` or unrecoverable terminal I/O failure.
pub fn run_config_menu(
    initial_config: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !stdin_is_interactive() {
        return Err(AppError::Usage(format!(
            "'{NAME} config' requires an interactive terminal"
        )));
    }
    let mut draft = initial_config.clone();
    let mut dirty = false;

    loop {
        let max_choice = render_config_screen(&draft, config_path, printer);
        let choice = match read_menu_choice_once(max_choice, 0) {
            Ok(val) => val,
            Err(AppError::Cancelled) => {
                eprintln!();
                printer.print_info("Operation cancelled by user.");
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        if choice == Some(0) {
            if try_save_and_exit(&draft, config_path, dirty, logger, printer)? {
                return Ok(());
            }
            if wait_for_enter().is_err() {
                return Ok(());
            }
            continue;
        }
        let should_pause = dispatch_config_choice(
            choice,
            max_choice,
            &mut draft,
            &mut dirty,
            config_path,
            logger,
            printer,
        )?;
        if should_pause && wait_for_enter().is_err() {
            return Ok(());
        }
    }
}

// =============================================================================
/// Resolves `choice` (`1..=N`) via [`CONFIG_MENU_ITEMS`]; returns `true` when screen output should pause.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn dispatch_config_choice(
    choice: Option<u8>,
    max_choice: u8,
    draft: &mut AppConfig,
    dirty: &mut bool,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    match resolve_menu_action(choice, CONFIG_MENU_ITEMS) {
        Some(ConfigMenuAction::ServerAndApi) => {
            *dirty |= edit_server_and_api_section(draft, printer)?;
        }
        Some(ConfigMenuAction::Fritzbox) => *dirty |= edit_fritzbox_section(draft, printer)?,
        Some(ConfigMenuAction::Weather) => *dirty |= edit_weather_section(draft, printer)?,
        Some(ConfigMenuAction::SystemAndLimits) => {
            *dirty |= edit_system_and_limits_section(draft, printer)?;
        }
        Some(ConfigMenuAction::TestFritzbox) => {
            run_draft_fritz_test(draft, config_path, logger, printer);
        }
        Some(ConfigMenuAction::EmergencyDisable) => {
            run_logged_tool(
                run_emergency_disable_automations(draft, logger, printer),
                printer,
            );
        }
        Some(ConfigMenuAction::BackupAndRestore) => {
            return run_backup_submenu(draft, config_path, logger, printer);
        }
        Some(ConfigMenuAction::ViewStatus) => {
            show_runtime_status(draft, config_path, false, logger, printer);
        }
        None => printer.print_error(&format!(
            "Invalid selection. Enter a number between 0 and {max_choice}."
        )),
    }
    Ok(true)
}

// =============================================================================
/// Opens the 2-item `Backup & Restore` selector; returns `false` when cancelled immediately.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn run_backup_submenu(
    draft: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    printer.print_subsection("Backup & Restore");
    match prompt_action_from_list(BACKUP_MENU_ITEMS, "Select action", printer)? {
        Some(BackupMenuAction::Export) => {
            run_logged_tool(run_interactive_export(draft, logger, printer), printer);
            Ok(true)
        }
        Some(BackupMenuAction::Import) => {
            run_logged_tool(
                run_interactive_import(Some(config_path), logger, printer),
                printer,
            );
            Ok(true)
        }
        None => Ok(false),
    }
}

// =============================================================================
/// Updates `logger` secret redaction with `draft` credentials and runs `cowhome test`.
fn run_draft_fritz_test(
    draft: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) {
    logger.reconfigure(
        logger.min_level(),
        draft.debug.console_colors,
        &[&draft.api.api_key, &draft.fritzbox.password],
    );
    run_logged_tool(
        run_fritz_connection_test(draft, config_path, logger, printer),
        printer,
    );
}

// =============================================================================
/// Prints non-cancellation errors from sub-tools without aborting the `config` menu.
fn run_logged_tool(res: AppResult<()>, printer: ConsolePrinter) {
    if let Err(err) = res {
        if !matches!(err, AppError::Cancelled) {
            printer.print_error(&err.to_string());
        }
    }
}

// =============================================================================
/// Renders `CowHome Configuration` banner, calm summary, and generated menu (`1..=N`).
fn render_config_screen(draft: &AppConfig, config_path: &Path, printer: ConsolePrinter) -> u8 {
    printer.clear_screen();
    printer.print_banner("Configuration");
    printer.print_subsection("Current Settings");
    let fb_user = if draft.fritzbox.username.trim().is_empty() {
        "default"
    } else {
        draft.fritzbox.username.as_str()
    };
    let vac_state = if draft.automation.vacation_mode {
        "on"
    } else {
        "off"
    };
    let rl_state = if draft.rate_limit.enabled {
        format!("{}/min", draft.rate_limit.max_requests_per_minute_per_ip)
    } else {
        String::from("off")
    };
    eprintln!("  Config File:    {}", config_path.display());
    eprintln!(
        "  Server & API:   {}:{} · Key: {}",
        draft.server.host,
        draft.server.port,
        format_api_key_badge(&draft.api.api_key, printer)
    );
    eprintln!(
        "  FritzBox:       {} · User: {fb_user} · Pass: {} · Poll: {}s",
        draft.fritzbox.host,
        mask_secret(&draft.fritzbox.password),
        draft.fritzbox.poll_interval
    );
    eprintln!(
        "  Weather:        {:.4}, {:.4} · {}",
        draft.weather.latitude, draft.weather.longitude, draft.weather.timezone
    );
    eprintln!(
        "  System:         Log: {} · Rate limit: {rl_state} · Vacation: {vac_state}",
        draft.debug.level
    );
    eprintln!();
    printer.print_action_menu(CONFIG_MENU_ITEMS, "Save & Exit")
}

// =============================================================================
/// Saves `draft` on exit, keeping the user in the editor if validation/write fails.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn try_save_and_exit(
    draft: &AppConfig,
    config_path: &Path,
    dirty: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    if !dirty {
        printer.print_info("No changes made.");
        return Ok(true);
    }
    let save_now = match prompt_confirm("Save changes?", true) {
        Ok(ans) => ans,
        Err(AppError::Cancelled) => return Ok(false),
        Err(err) => return Err(err),
    };
    if !save_now {
        printer.print_info("Changes discarded.");
        return Ok(true);
    }
    match save_config(draft, config_path) {
        Ok(()) => {
            logger.info(&format!(
                "Configuration saved to '{}'",
                config_path.display()
            ));
            printer.print_success("Configuration saved successfully.");
            Ok(true)
        }
        Err(err) => {
            printer.print_error(&err.to_string());
            let discard =
                prompt_confirm("Discard unsaved changes and exit anyway?", false).unwrap_or(false);
            Ok(discard)
        }
    }
}
