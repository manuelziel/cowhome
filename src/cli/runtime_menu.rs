//! Frequency-ordered interactive `Runtime` main menu (`cowhome` without arguments).
//!
//! Strictly adheres to `_project-resources/docs/design_guidelines.md` Section 8:
//! dynamically numbers options from [`RUNTIME_MENU_ITEMS`] (`Status`, `Configure`,
//! `Start`, `Stop`, `Restart`, `Validate Configuration`, `Setup / Maintenance`, `Help`).

use std::path::Path;
use std::time::Duration;

use crate::cli::args::print_help;
use crate::cli::config_menu::run_config_menu;
use crate::cli::input::{read_menu_choice_once, resolve_menu_action, wait_for_enter};
use crate::cli::output::ConsolePrinter;
use crate::cli::setup_menu::run_setup_menu;
use crate::cli::status_view::{show_runtime_header_status, show_runtime_status};
use crate::core::config::{load_runtime_config_lenient, validate_config_file, AppConfig};
use crate::core::constants::{
    fhs_paths::SYSTEMD_UNIT_NAME,
    service_defaults::{ELEVATED_CONFIG_TIMEOUT_SECS, SUDO_TIMEOUT_SECS},
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::{LogLevel, Logger};
use crate::core::process::run_command_inherited;
use crate::setup::systemd::control_service_unit;

// =============================================================================
/// Typed actions in the frequency-ordered `Runtime` main menu (`design_guidelines.md` § 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeMenuAction {
    /// Show service, storage, and live device status.
    Status,
    /// Open interactive configuration & sysadmin manager.
    Configure,
    /// Start `cowhome.service`.
    Start,
    /// Stop `cowhome.service`.
    Stop,
    /// Restart `cowhome.service`.
    Restart,
    /// Validate `config.toml` syntax, schema, and security policy.
    Validate,
    /// Open interactive system setup / maintenance menu.
    Setup,
    /// Show CLI usage help.
    Help,
}

/// Canonical operational menu order (`design_guidelines.md` § 8).
const RUNTIME_MENU_ITEMS: &[(RuntimeMenuAction, &str)] = &[
    (RuntimeMenuAction::Status, "Status"),
    (RuntimeMenuAction::Configure, "Configure"),
    (RuntimeMenuAction::Start, "Start"),
    (RuntimeMenuAction::Stop, "Stop"),
    (RuntimeMenuAction::Restart, "Restart"),
    (RuntimeMenuAction::Validate, "Validate Configuration"),
    (RuntimeMenuAction::Setup, "Setup / Maintenance"),
    (RuntimeMenuAction::Help, "Help"),
];

// =============================================================================
/// Borrowed context passed to a single interactive `Runtime` menu dispatch step.
struct RuntimeActionCtx<'a> {
    /// Loaded (or fallback) runtime configuration.
    config: &'a AppConfig,
    /// Resolved configuration path.
    config_path: &'a Path,
    /// Whether `config_path` is unreadable without `sudo` (`0750`/`0640`).
    cfg_locked: bool,
    /// CLI `--debug` override flag.
    debug_override: bool,
    /// Active logger instance.
    logger: &'a Logger,
    /// Console renderer.
    printer: ConsolePrinter,
}

// =============================================================================
/// Runs the frequency-ordered Runtime TUI loop until `0`, `EOF`, or `Ctrl+C`.
///
/// # Errors
/// Returns [`AppError`] only on fatal unrecoverable terminal stream errors.
pub fn run_runtime_menu(debug_override: bool) -> AppResult<()> {
    loop {
        let (config, config_path, cfg_locked) = load_runtime_config_lenient(None)?;
        let level = if debug_override {
            LogLevel::Debug
        } else {
            config.debug.level
        };
        let logger = Logger::new(level, config.debug.console_colors);
        if !cfg_locked {
            logger.reconfigure(
                level,
                config.debug.console_colors,
                &[&config.api.api_key, &config.fritzbox.password],
            );
        }
        let printer = ConsolePrinter::new(logger.colors_enabled());

        let max_choice = render_runtime_menu_screen(&config_path, printer);
        let choice = match read_menu_choice_once(max_choice, 0) {
            Ok(val) => val,
            Err(AppError::Cancelled) => {
                printer.print_info("Input closed (EOF). Exiting interactive menu.");
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        eprintln!();

        let ctx = RuntimeActionCtx {
            config: &config,
            config_path: &config_path,
            cfg_locked,
            debug_override,
            logger: &logger,
            printer,
        };
        if !handle_menu_selection(choice, max_choice, &ctx)? {
            return Ok(());
        }
    }
}

// =============================================================================
/// Clears screen and renders the `CowHome Runtime` banner, status, and generated options.
fn render_runtime_menu_screen(config_path: &Path, printer: ConsolePrinter) -> u8 {
    printer.clear_screen();
    printer.print_banner("Runtime");
    show_runtime_header_status(config_path, printer);
    eprintln!();
    printer.print_action_menu(RUNTIME_MENU_ITEMS, "Exit")
}

// =============================================================================
/// Routes `choice`, returning `Ok(false)` when the user chooses to exit the menu.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn handle_menu_selection(
    choice: Option<u8>,
    max_choice: u8,
    ctx: &RuntimeActionCtx<'_>,
) -> AppResult<bool> {
    if choice == Some(0) {
        ctx.printer.print_info("Exiting interactive menu.");
        return Ok(false);
    }
    match resolve_menu_action(choice, RUNTIME_MENU_ITEMS) {
        Some(RuntimeMenuAction::Configure) => {
            if let Err(err) = launch_config_editor(ctx) {
                report_non_cancelled(&err, ctx.printer);
                return Ok(wait_for_enter().is_ok());
            }
            return Ok(true);
        }
        Some(RuntimeMenuAction::Setup) => {
            if let Err(err) = run_setup_menu(false, ctx.debug_override, ctx.logger, ctx.printer) {
                report_non_cancelled(&err, ctx.printer);
                return Ok(wait_for_enter().is_ok());
            }
            return Ok(true);
        }
        Some(act) => execute_paused_action(act, ctx),
        None => ctx.printer.print_error(&format!(
            "Invalid selection. Enter a number between 0 and {max_choice}."
        )),
    }
    Ok(wait_for_enter().is_ok())
}

// =============================================================================
/// Opens `cowhome config` directly or elevates via `sudo` when `/etc/cowhome` is `0750`.
///
/// # Errors
/// Returns [`AppError`] if interactive editing or `sudo` elevation fails.
fn launch_config_editor(ctx: &RuntimeActionCtx<'_>) -> AppResult<()> {
    if !ctx.cfg_locked {
        return run_config_menu(ctx.config, ctx.config_path, ctx.logger, ctx.printer);
    }
    elevate_subcommand_via_sudo("config", None, ctx.debug_override, ctx.printer)
}

// =============================================================================
/// Executes non-nested actions (`Status`, `Start`, `Stop`, `Restart`, `Validate`, `Help`).
fn execute_paused_action(action: RuntimeMenuAction, ctx: &RuntimeActionCtx<'_>) {
    let result: AppResult<()> = match action {
        RuntimeMenuAction::Status => {
            show_runtime_status(
                ctx.config,
                ctx.config_path,
                ctx.cfg_locked,
                ctx.logger,
                ctx.printer,
            );
            Ok(())
        }
        RuntimeMenuAction::Start => control_service_unit("start", ctx.logger).map(|()| {
            ctx.printer
                .print_success(&format!("Started {SYSTEMD_UNIT_NAME}"))
        }),
        RuntimeMenuAction::Stop => control_service_unit("stop", ctx.logger).map(|()| {
            ctx.printer
                .print_success(&format!("Stopped {SYSTEMD_UNIT_NAME}"))
        }),
        RuntimeMenuAction::Restart => control_service_unit("restart", ctx.logger).map(|()| {
            ctx.printer
                .print_success(&format!("Restarted {SYSTEMD_UNIT_NAME}"))
        }),
        RuntimeMenuAction::Validate if ctx.cfg_locked => {
            elevate_subcommand_via_sudo("validate", None, ctx.debug_override, ctx.printer)
        }
        RuntimeMenuAction::Validate => validate_config_file(None).map(|(_, p)| {
            ctx.printer
                .print_success(&format!("Configuration '{}' is valid", p.display()));
        }),
        RuntimeMenuAction::Help => {
            print_help();
            Ok(())
        }
        RuntimeMenuAction::Configure | RuntimeMenuAction::Setup => Ok(()),
    };
    if let Err(err) = result {
        report_non_cancelled(&err, ctx.printer);
    }
}

// =============================================================================
/// Re-invokes `<current_exe> <subcommand>` under `sudo` from an interactive TTY.
///
/// # Errors
/// Returns [`AppError`] if resolving `current_exe` or executing `sudo` fails.
pub fn elevate_subcommand_via_sudo(
    subcommand: &str,
    explicit_file: Option<&Path>,
    debug: bool,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let exe = std::env::current_exe()
        .map_err(|err| AppError::io("Failed resolving current binary path for sudo", err))?;
    let exe_str = exe
        .to_str()
        .ok_or_else(|| AppError::Config("Current executable path is not valid UTF-8".into()))?;
    printer.print_info("Elevating privileges via sudo...");
    let mut args: Vec<&str> = vec!["--", exe_str, subcommand];
    if let Some(cfg_str) = explicit_file.and_then(Path::to_str) {
        args.push("--file");
        args.push(cfg_str);
    }
    if debug {
        args.push("--debug");
    }
    let timeout = Duration::from_secs(ELEVATED_CONFIG_TIMEOUT_SECS.max(SUDO_TIMEOUT_SECS));
    if !run_command_inherited("sudo", &args, timeout)? {
        return Err(AppError::Auth(format!(
            "Elevated '{subcommand}' did not complete successfully"
        )));
    }
    Ok(())
}

// =============================================================================
/// Emits an error message on `stderr` unless the error was a clean user cancellation.
fn report_non_cancelled(err: &AppError, printer: ConsolePrinter) {
    if !matches!(err, AppError::Cancelled) {
        printer.print_error(&err.to_string());
    }
}
