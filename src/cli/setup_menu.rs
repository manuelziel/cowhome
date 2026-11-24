//! Context-sensitive interactive `Setup` TUI menu and subcommand dispatcher.
//!
//! Renders the 63-column `<App> Setup` banner, live status rows (`Software`,
//! `Dependencies`, `Internet`), and dynamically numbered context-sensitive options,
//! delegating privileged mutations to [`super::setup_ops`].

use crate::cli::input::{
    read_menu_choice_once, resolve_menu_action, stdin_is_interactive, wait_for_enter,
};
use crate::cli::output::{ConsolePrinter, write_stdout_line};
pub use crate::cli::setup_args::SetupAction;
use crate::cli::setup_ops::{
    SetupRunOpts, run_install, run_purge, run_reinstall, run_uninstall, run_update,
};
use crate::cli::status_view::{
    show_detailed_setup_status, show_detailed_version, show_setup_header_status,
    show_system_requirements,
};
use crate::core::constants::app_meta::{NAME, SOFTWARE_NAME, VERSION};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::setup::installer::is_installed;

// =============================================================================
/// Control-flow outcome after processing a single interactive Setup menu selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuOutcome {
    /// Remain inside the interactive Setup menu after `Press Enter to continue...`.
    Continue,
    /// Pause for acknowledgment (`Press Enter to continue...`) and then exit the menu.
    PauseAndExit,
    /// Exit the interactive Setup menu immediately (`0) Exit`).
    ExitImmediate,
}

// =============================================================================
/// Typed actions rendered in the interactive `Setup` menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupMenuItem {
    /// Fresh installation (`!installed`).
    Install,
    /// Zero-data-loss binary & unit update (`installed`).
    Update,
    /// Full reinstallation preserving config & DB (`installed`).
    Reinstall,
    /// Remove binary & unit while keeping `/etc` and `/var/lib` (`installed`).
    Uninstall,
    /// Completely remove all files, config, data, and service user (`installed`).
    Purge,
    /// Display FHS, dependency, network, and service details (`installed`).
    DetailedStatus,
    /// Display OS & package requirements (`!installed`).
    Requirements,
    /// Display detailed version & license metadata (`!installed`).
    VersionInfo,
}

/// Dynamically numbered options shown when CowHome is already installed.
const INSTALLED_MENU_ITEMS: &[(SetupMenuItem, &str)] = &[
    (SetupMenuItem::Update, "Update"),
    (SetupMenuItem::Reinstall, "Reinstall"),
    (SetupMenuItem::Uninstall, "Uninstall"),
    (SetupMenuItem::Purge, "Purge"),
    (SetupMenuItem::DetailedStatus, "Status"),
];

/// Dynamically numbered options shown before CowHome is installed.
const UNINSTALLED_MENU_ITEMS: &[(SetupMenuItem, &str)] = &[
    (SetupMenuItem::Install, "Install"),
    (SetupMenuItem::Requirements, "Requirements"),
    (SetupMenuItem::VersionInfo, "Version"),
];

// =============================================================================
/// Executes a parsed [`SetupAction`] from CLI arguments.
///
/// # Errors
/// Returns [`AppError`] on missing privileges or setup errors.
pub fn execute_setup_action(
    action: SetupAction,
    quiet: bool,
    debug: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let opts = SetupRunOpts { quiet, debug };
    match action {
        SetupAction::Interactive { skip_deps } => run_setup_menu(skip_deps, debug, logger, printer),
        SetupAction::Status { skip_deps } => {
            if !quiet {
                show_detailed_setup_status(skip_deps, printer);
            }
            Ok(())
        }
        SetupAction::Requirements => {
            if !quiet {
                show_system_requirements(printer);
            }
            Ok(())
        }
        SetupAction::Version => {
            show_detailed_version();
            Ok(())
        }
        SetupAction::Help => {
            print_setup_help();
            Ok(())
        }
        SetupAction::Install { force, skip_deps } => {
            run_install(force, skip_deps, opts, logger, printer)
        }
        SetupAction::Update { skip_deps } => run_update(skip_deps, opts, logger, printer),
        SetupAction::Reinstall { skip_deps } => run_reinstall(skip_deps, opts, logger, printer),
        SetupAction::Uninstall { force } => run_uninstall(force, opts, logger, printer).map(|_| ()),
        SetupAction::Purge { force } => run_purge(force, opts, logger, printer).map(|_| ()),
    }
}

// =============================================================================
/// Runs the context-sensitive interactive Setup TUI matching `02_linux/_project_template`.
///
/// # Errors
/// Returns [`AppError`] if `stdin` is non-interactive or terminal I/O fails.
pub fn run_setup_menu(
    skip_deps: bool,
    debug: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !stdin_is_interactive() {
        return Err(AppError::Usage(
            "Interactive setup menu requires a TTY (specify 'install', 'status', etc.)".into(),
        ));
    }
    let opts = SetupRunOpts {
        quiet: false,
        debug,
    };
    loop {
        let items = if is_installed() {
            INSTALLED_MENU_ITEMS
        } else {
            UNINSTALLED_MENU_ITEMS
        };
        let max_choice = render_setup_menu_screen(items, skip_deps, printer);
        let Some(outcome) = step_setup_menu(items, max_choice, skip_deps, opts, logger, printer)?
        else {
            return Ok(());
        };
        match outcome {
            MenuOutcome::ExitImmediate => return Ok(()),
            MenuOutcome::PauseAndExit => {
                if let Err(_cancelled) = wait_for_enter() {
                    // Terminal closed after action finished; exit menu cleanly.
                }
                return Ok(());
            }
            MenuOutcome::Continue => {
                if wait_for_enter().is_err() {
                    return Ok(());
                }
            }
        }
    }
}

// =============================================================================
/// Reads and executes one interactive Setup menu iteration (`None` on `EOF`).
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn step_setup_menu(
    items: &[(SetupMenuItem, &str)],
    max_choice: u8,
    skip_deps: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<Option<MenuOutcome>> {
    let choice_opt = match read_menu_choice_once(max_choice, 0) {
        Ok(val) => val,
        Err(AppError::Cancelled) => {
            printer.print_info("Input closed (EOF). Exiting setup menu.");
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    eprintln!();
    if choice_opt == Some(0) {
        printer.print_info("Cancelled by user.");
        return Ok(Some(MenuOutcome::ExitImmediate));
    }
    let outcome = match resolve_menu_action(choice_opt, items) {
        Some(item) => dispatch_setup_item(item, skip_deps, opts, logger, printer),
        None => {
            printer.print_error(&format!(
                "Invalid selection. Enter a number between 0 and {max_choice}."
            ));
            MenuOutcome::Continue
        }
    };
    Ok(Some(outcome))
}

// =============================================================================
/// Renders the Setup banner, status lines, and dynamically numbered options.
fn render_setup_menu_screen(
    items: &[(SetupMenuItem, &str)],
    skip_deps: bool,
    printer: ConsolePrinter,
) -> u8 {
    printer.clear_screen();
    printer.print_banner("Setup");
    show_setup_header_status(skip_deps, printer);
    eprintln!();
    printer.print_action_menu(items, "Exit")
}

// =============================================================================
/// Executes a resolved [`SetupMenuItem`] inside the interactive Setup menu.
fn dispatch_setup_item(
    item: SetupMenuItem,
    skip_deps: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> MenuOutcome {
    match item {
        SetupMenuItem::Install => map_mutation_outcome(
            run_install(false, skip_deps, opts, logger, printer),
            printer,
        ),
        SetupMenuItem::Update => {
            map_mutation_outcome(run_update(skip_deps, opts, logger, printer), printer)
        }
        SetupMenuItem::Reinstall => {
            map_mutation_outcome(run_reinstall(skip_deps, opts, logger, printer), printer)
        }
        SetupMenuItem::Uninstall => {
            map_confirmable_outcome(run_uninstall(false, opts, logger, printer), printer)
        }
        SetupMenuItem::Purge => {
            map_confirmable_outcome(run_purge(false, opts, logger, printer), printer)
        }
        SetupMenuItem::DetailedStatus => {
            show_detailed_setup_status(skip_deps, printer);
            MenuOutcome::Continue
        }
        SetupMenuItem::Requirements => {
            show_system_requirements(printer);
            MenuOutcome::Continue
        }
        SetupMenuItem::VersionInfo => {
            show_detailed_version();
            MenuOutcome::Continue
        }
    }
}

// =============================================================================
/// Maps a setup mutation result to `PauseAndExit` on success or stays in menu on error.
fn map_mutation_outcome(result: AppResult<()>, printer: ConsolePrinter) -> MenuOutcome {
    match result {
        Ok(()) => MenuOutcome::PauseAndExit,
        Err(AppError::Cancelled) => MenuOutcome::Continue,
        Err(err) => {
            printer.print_error(&err.to_string());
            MenuOutcome::Continue
        }
    }
}

// =============================================================================
/// Maps a confirmable teardown result (`true` = executed, `false` = cancelled).
fn map_confirmable_outcome(result: AppResult<bool>, printer: ConsolePrinter) -> MenuOutcome {
    match result {
        Ok(true) => MenuOutcome::PauseAndExit,
        Ok(false) | Err(AppError::Cancelled) => MenuOutcome::Continue,
        Err(err) => {
            printer.print_error(&err.to_string());
            MenuOutcome::Continue
        }
    }
}

// =============================================================================
/// Writes `setup.sh` / `<name> setup` usage (`<= 78` cols) consistently to `stdout`.
pub fn print_setup_help() {
    write_stdout_line(&format!(
        "\
═══ {SOFTWARE_NAME} Setup v{VERSION} ═══

Usage: ./setup.sh [ACTION] [OPTIONS]
   or: {NAME} setup [ACTION] [OPTIONS]

Actions:
  install                     Install {SOFTWARE_NAME}
  update, --upgrade           Update binary & unit, keeping config & data
  reinstall, --reinstall      Reinstall system files, keeping config & data
  uninstall, --remove         Remove binary & unit, keeping config & data
  purge, --purge              Remove everything including config & data
  status, --info              Show installation status
  requirements                Show OS & package requirements
  help, --help, -h            Show this help
  version, --version, -v      Show version metadata

Options:
  --interactive, -i           Launch interactive setup menu
  --quiet, -q                 Suppress non-error output
  --force, -f                 Skip confirmation prompts
  --no-deps                   Skip dependency checks
  --debug                     Enable debug output"
    ));
}
