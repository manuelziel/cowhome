//! Interactive TUI configuration & sysadmin manager (`cowhome config` and `Runtime -> Configure`).
//!
//! Follows `_project-resources/docs/design_guidelines.md` Section 8: uses concise noun
//! labels (`Server`, `FritzBox`, `Weather`, `Limits`, `Connection Test`, `Automations`,
//! `Backup`), dynamic `Save Changes` when modified, `[y/N]`-guarded `Reset to Defaults`,
//! and context-sensitive `0) Back` vs `0) Exit`.

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
use crate::cli::status_view::format_api_key_badge;
use crate::core::config::{AppConfig, save_config};
use crate::core::constants::{
    app_meta::{NAME, SOFTWARE_NAME},
    cli_style::{ANSI_GREEN, ANSI_YELLOW},
    fhs_paths::SYSTEMD_UNIT_NAME,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::validation::mask_secret;
use crate::setup::systemd::{control_service_unit, query_unit_status};

// =============================================================================
/// Typed actions exposed in the dynamically numbered `cowhome config` menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMenuAction {
    /// Edit HTTP bind address, port, base URL, API key, and trusted proxies.
    Server,
    /// Edit FritzBox host, credentials, and polling interval.
    Fritzbox,
    /// Edit home coordinates, IANA timezone, and forecast horizon.
    Weather,
    /// Edit rate limits, automation/daemon intervals, DB path, or logging.
    Limits,
    /// Run live `TR-064` & `AHA` connection test with current working settings.
    ConnectionTest,
    /// Break-glass shutdown of all active automations across scopes.
    Automations,
    /// Export or import SQLite backups (`YAML` / `JSON` / `TOML`).
    Backup,
    /// Validate and persist pending modifications to `config.toml`.
    SaveChanges,
    /// Restore all settings in memory to built-in defaults (`[y/N]`).
    ResetDefaults,
}

/// Canonical noun-labeled domain sections & admin tools (`design_guidelines.md` § 8).
const BASE_CONFIG_ITEMS: &[(ConfigMenuAction, &str)] = &[
    (ConfigMenuAction::Server, "Server"),
    (ConfigMenuAction::Fritzbox, "FritzBox"),
    (ConfigMenuAction::Weather, "Weather"),
    (ConfigMenuAction::Limits, "Limits"),
    (ConfigMenuAction::ConnectionTest, "Connection Test"),
    (ConfigMenuAction::Automations, "Automations"),
    (ConfigMenuAction::Backup, "Backup"),
];

// =============================================================================
/// Typed actions inside the `Backup` sub-selector (`0) Cancel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupMenuAction {
    /// Export SQLite backup or configuration file.
    Export,
    /// Import SQLite backup or configuration file.
    Import,
}

/// Ordered `(action, label)` slice for `Backup`.
const BACKUP_MENU_ITEMS: &[(BackupMenuAction, &str)] = &[
    (BackupMenuAction::Export, "Export Backup"),
    (BackupMenuAction::Import, "Import Backup"),
];

// =============================================================================
/// Mutable configuration state tracking persisted vs. in-memory settings.
struct ConfigEditorState {
    /// Last persisted configuration snapshot on disk.
    saved: AppConfig,
    /// Working configuration edited interactively before saving.
    working: AppConfig,
}

impl ConfigEditorState {
    /// Returns `true` when `working` differs from the persisted `saved` state.
    fn is_dirty(&self) -> bool {
        self.working != self.saved
    }
}

// =============================================================================
/// Runs the interactive configuration editor (`nested = true` -> `0) Back`, `false` -> `0) Exit`).
///
/// # Errors
/// Returns [`AppError`] on non-interactive `stdin` or unrecoverable terminal I/O failure.
pub fn run_config_menu(
    initial_config: &AppConfig,
    config_path: &Path,
    nested: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !stdin_is_interactive() {
        return Err(AppError::Usage(format!(
            "'{NAME} config' requires an interactive terminal"
        )));
    }
    let mut state = ConfigEditorState {
        saved: initial_config.clone(),
        working: initial_config.clone(),
    };
    let exit_label = if nested { "Back" } else { "Exit" };

    loop {
        let items = build_config_menu_items(state.is_dirty());
        let max_choice = render_config_screen(&state, config_path, &items, exit_label, printer);
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
            if confirm_leave_menu(state.is_dirty(), printer)? {
                return Ok(());
            }
            continue;
        }
        let should_pause = dispatch_config_choice(
            choice,
            max_choice,
            &items,
            &mut state,
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
/// Builds the dynamic `(ConfigMenuAction, &str)` list, inserting `Save Changes` when dirty.
fn build_config_menu_items(is_dirty: bool) -> Vec<(ConfigMenuAction, &'static str)> {
    let mut items = BASE_CONFIG_ITEMS.to_vec();
    if is_dirty {
        items.push((ConfigMenuAction::SaveChanges, "Save Changes"));
    }
    items.push((ConfigMenuAction::ResetDefaults, "Reset to Defaults"));
    items
}

// =============================================================================
/// Guards `0) Back` / `0) Exit` with `Discard unsaved changes? [y/N]` when `is_dirty`.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream failure.
fn confirm_leave_menu(is_dirty: bool, printer: ConsolePrinter) -> AppResult<bool> {
    if !is_dirty {
        return Ok(true);
    }
    eprintln!();
    match prompt_confirm("Discard unsaved changes?", false) {
        Ok(true) => {
            printer.print_info("Unsaved changes discarded.");
            Ok(true)
        }
        Ok(false) | Err(AppError::Cancelled) => Ok(false),
        Err(err) => Err(err),
    }
}

// =============================================================================
/// Resolves `choice` (`1..=N`) against `items`; returns `true` when screen output should pause.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn dispatch_config_choice(
    choice: Option<u8>,
    max_choice: u8,
    items: &[(ConfigMenuAction, &str)],
    state: &mut ConfigEditorState,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    match resolve_menu_action(choice, items) {
        Some(ConfigMenuAction::Server) => {
            edit_server_and_api_section(&mut state.working, printer)?;
        }
        Some(ConfigMenuAction::Fritzbox) => {
            edit_fritzbox_section(&mut state.working, printer)?;
        }
        Some(ConfigMenuAction::Weather) => {
            edit_weather_section(&mut state.working, printer)?;
        }
        Some(ConfigMenuAction::Limits) => {
            return edit_system_and_limits_section(&mut state.working, printer);
        }
        Some(ConfigMenuAction::ConnectionTest) => {
            run_working_fritz_test(&state.working, config_path, logger, printer);
        }
        Some(ConfigMenuAction::Automations) => {
            run_logged_tool(
                run_emergency_disable_automations(&state.working, logger, printer),
                printer,
            );
        }
        Some(ConfigMenuAction::Backup) => {
            return run_backup_submenu(&state.working, config_path, logger, printer);
        }
        Some(ConfigMenuAction::SaveChanges) => {
            save_working_config(state, config_path, logger, printer)?;
        }
        Some(ConfigMenuAction::ResetDefaults) => {
            reset_to_defaults(&mut state.working, printer)?;
        }
        None => printer.print_error(&format!(
            "Invalid selection. Enter a number between 0 and {max_choice}."
        )),
    }
    Ok(true)
}

// =============================================================================
/// Opens the 2-item `Backup` selector; returns `false` when cancelled immediately.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn run_backup_submenu(
    working: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    printer.print_subsection("Backup");
    match prompt_action_from_list(BACKUP_MENU_ITEMS, "Select action", printer)? {
        Some(BackupMenuAction::Export) => {
            run_logged_tool(run_interactive_export(working, logger, printer), printer);
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
/// Updates `logger` secret redaction with `working` credentials and runs `cowhome test`.
fn run_working_fritz_test(
    working: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) {
    logger.reconfigure(
        logger.min_level(),
        working.debug.console_colors,
        &[&working.api.api_key, &working.fritzbox.password],
    );
    run_logged_tool(
        run_fritz_connection_test(working, config_path, logger, printer),
        printer,
    );
}

// =============================================================================
/// Prints non-cancellation errors from sub-tools without aborting the `config` menu.
fn run_logged_tool(res: AppResult<()>, printer: ConsolePrinter) {
    if let Err(err) = res
        && !matches!(err, AppError::Cancelled)
    {
        printer.print_error(&err.to_string());
    }
}

// =============================================================================
/// Renders `═══ CowHome Configuration ═══`, calm summary, state badge, and options.
fn render_config_screen(
    state: &ConfigEditorState,
    config_path: &Path,
    items: &[(ConfigMenuAction, &str)],
    exit_label: &str,
    printer: ConsolePrinter,
) -> u8 {
    let cfg = &state.working;
    let state_badge = if state.is_dirty() {
        printer.colorize("[UNSAVED CHANGES]", ANSI_YELLOW)
    } else {
        printer.colorize("[saved]", ANSI_GREEN)
    };
    let fb_user = if cfg.fritzbox.username.trim().is_empty() {
        "default"
    } else {
        cfg.fritzbox.username.as_str()
    };
    let vac_state = if cfg.automation.vacation_mode {
        "on"
    } else {
        "off"
    };
    let rl_state = if cfg.rate_limit.enabled {
        format!("{}/min", cfg.rate_limit.max_requests_per_minute_per_ip)
    } else {
        String::from("off")
    };
    printer.clear_screen();
    printer.print_header(&format!("{SOFTWARE_NAME} Configuration"));
    eprintln!("  Config File:    {} {state_badge}", config_path.display());
    eprintln!(
        "  Server:         {}:{} · Key: {}",
        cfg.server.host,
        cfg.server.port,
        format_api_key_badge(&cfg.api.api_key, printer)
    );
    eprintln!(
        "  FritzBox:       {} · User: {fb_user} · Pass: {} · Poll: {}s",
        cfg.fritzbox.host,
        mask_secret(&cfg.fritzbox.password),
        cfg.fritzbox.poll_interval
    );
    eprintln!(
        "  Weather:        {:.4}, {:.4} · {} ({}d)",
        cfg.weather.latitude,
        cfg.weather.longitude,
        cfg.weather.timezone,
        cfg.weather.forecast_days
    );
    eprintln!(
        "  Limits:         Log: {} · Rate limit: {rl_state} · Vacation: {vac_state}\n",
        cfg.debug.level
    );
    printer.print_action_menu(items, exit_label)
}

// =============================================================================
/// Validates and atomically writes `state.working` to `config_path`, then offers reload/restart.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors during the reload prompt.
fn save_working_config(
    state: &mut ConfigEditorState,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    eprintln!();
    let needs_restart = state.working.database != state.saved.database
        || state.working.server != state.saved.server
        || state.working.daemon != state.saved.daemon;
    if let Err(err) = save_config(&state.working, config_path) {
        printer.print_error(&err.to_string());
        return Ok(());
    }
    state.saved = state.working.clone();
    logger.info(&format!(
        "Configuration saved to '{}'",
        config_path.display()
    ));
    printer.print_success("Configuration saved successfully.");
    offer_active_service_reload(needs_restart, logger, printer)
}

// =============================================================================
/// Prompts to `reload` (`SIGHUP`) or `restart` `cowhome.service` when the unit is active.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn offer_active_service_reload(
    needs_restart: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if query_unit_status().active_state != "active" {
        return Ok(());
    }
    let (verb, question, done_msg) = if needs_restart {
        (
            "restart",
            format!("Restart {SYSTEMD_UNIT_NAME} now to apply server/daemon/database changes?"),
            format!("Restarted {SYSTEMD_UNIT_NAME}."),
        )
    } else {
        (
            "reload",
            format!("Reload {SYSTEMD_UNIT_NAME} now (SIGHUP) to apply changes?"),
            format!("Reloaded {SYSTEMD_UNIT_NAME}."),
        )
    };
    if let Ok(true) = prompt_confirm(&question, true) {
        match control_service_unit(verb, logger) {
            Ok(()) => printer.print_success(&done_msg),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
    Ok(())
}

// =============================================================================
/// Restores `working` to [`AppConfig::default`] after destructive `[y/N]` confirmation.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn reset_to_defaults(working: &mut AppConfig, printer: ConsolePrinter) -> AppResult<()> {
    eprintln!();
    let defaults = AppConfig::default();
    if *working == defaults {
        printer.print_info("Settings already match defaults.");
        return Ok(());
    }
    match prompt_confirm("Reset all settings to defaults?", false) {
        Ok(true) => {
            *working = defaults;
            printer.print_info("Settings reset to defaults (select 'Save Changes' to apply).");
            Ok(())
        }
        Ok(false) | Err(AppError::Cancelled) => {
            printer.print_info("Reset cancelled.");
            Ok(())
        }
        Err(err) => Err(err),
    }
}
