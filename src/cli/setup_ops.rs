//! Privileged setup action runners (`install`, `update`, `reinstall`, `uninstall`, `purge`).
//!
//! Separated from [`super::setup_menu`] so interactive menu rendering and privileged
//! `sudo` handoff / confirmation workflows remain focused (`<= 300` lines per file).

use std::time::Duration;

use crate::cli::input::{prompt_confirm, stdin_is_interactive};
use crate::cli::output::ConsolePrinter;
use crate::core::constants::{
    app_meta::{NAME, SOFTWARE_NAME},
    fhs_paths::{CONFIG_DIR, STATE_DIR, SYSTEMD_UNIT_NAME},
    service_defaults::{ELEVATED_SETUP_TIMEOUT_SECS, SUDO_TIMEOUT_SECS},
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::process::run_command_inherited;
use crate::core::security::is_root;
use crate::setup::installer::{self, is_installed};

// =============================================================================
/// Shared execution options (`quiet`, `debug`) passed through Setup workflows.
#[derive(Debug, Clone, Copy)]
pub struct SetupRunOpts {
    /// Suppress non-error progress banners and steps (`--quiet` / `-q`).
    pub quiet: bool,
    /// Propagate `--debug` across `sudo` re-invocations.
    pub debug: bool,
}

// =============================================================================
/// Executes `install` and prints post-installation `Next steps:`.
///
/// # Errors
/// Returns [`AppError`] if root elevation or installation fails.
pub fn run_install(
    force: bool,
    skip_deps: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !is_root() {
        return delegate_or_reject_root("install", force, skip_deps, opts, printer);
    }
    if !opts.quiet {
        printer.print_header(&format!("Installing {SOFTWARE_NAME}"));
    }
    let mut observer = |ev| {
        if !opts.quiet {
            printer.render_setup_event(&ev);
        }
    };
    installer::install(force, skip_deps, logger, &mut observer)?;
    if !opts.quiet {
        print_install_next_steps(printer);
    }
    Ok(())
}

// =============================================================================
/// Prints post-install guidance (`sudo cowhome config`, `test`, `enable --now`, `journalctl`).
fn print_install_next_steps(printer: ConsolePrinter) {
    printer.print_success(&format!("{SOFTWARE_NAME} installed successfully!"));
    eprintln!();
    printer.print_info("Next steps:");
    let steps = [
        format!("Configure: sudo {NAME} config"),
        format!("Test FritzBox: sudo {NAME} test"),
        format!("Enable & start: sudo systemctl enable --now {SYSTEMD_UNIT_NAME}"),
        format!("View logs: sudo journalctl -u {SYSTEMD_UNIT_NAME} -f"),
    ];
    for (idx, step) in steps.iter().enumerate() {
        eprintln!("  {}. {step}", idx + 1);
    }
}

// =============================================================================
/// Executes zero-data-loss `update`.
///
/// # Errors
/// Returns [`AppError`] if root elevation or update fails.
pub fn run_update(
    skip_deps: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !is_root() {
        return delegate_or_reject_root("update", false, skip_deps, opts, printer);
    }
    if !opts.quiet {
        printer.print_header(&format!("Updating {SOFTWARE_NAME}"));
    }
    let mut observer = |ev| {
        if !opts.quiet {
            printer.render_setup_event(&ev);
        }
    };
    installer::update(skip_deps, logger, &mut observer)?;
    if !opts.quiet {
        printer.print_success("Update completed successfully!");
    }
    Ok(())
}

// =============================================================================
/// Executes `reinstall`.
///
/// # Errors
/// Returns [`AppError`] if root elevation or reinstallation fails.
pub fn run_reinstall(
    skip_deps: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !is_root() {
        return delegate_or_reject_root("reinstall", false, skip_deps, opts, printer);
    }
    if !opts.quiet {
        printer.print_header(&format!("Reinstalling {SOFTWARE_NAME}"));
    }
    let mut observer = |ev| {
        if !opts.quiet {
            printer.render_setup_event(&ev);
        }
    };
    installer::reinstall(skip_deps, logger, &mut observer)?;
    if !opts.quiet {
        printer.print_success("Reinstallation completed successfully!");
    }
    Ok(())
}

// =============================================================================
/// Executes `uninstall` after `[y/N]` confirmation unless `force` is set.
///
/// # Errors
/// Returns [`AppError`] if root elevation, confirmation, or uninstallation fails.
pub fn run_uninstall(
    force: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    if !is_root() {
        delegate_or_reject_root("uninstall", force, false, opts, printer)?;
        return Ok(true);
    }
    if !opts.quiet {
        printer.print_header(&format!("Uninstalling {SOFTWARE_NAME}"));
    }
    if !is_installed() {
        if !opts.quiet {
            printer.print_warning(&format!("{SOFTWARE_NAME} is not installed"));
        }
        return Ok(false);
    }
    if !confirm_destructive(
        &format!("Are you sure you want to uninstall {SOFTWARE_NAME}?"),
        "Uninstall cancelled",
        force,
        opts.quiet,
        printer,
    )? {
        return Ok(false);
    }
    let mut observer = |ev| {
        if !opts.quiet {
            printer.render_setup_event(&ev);
        }
    };
    installer::uninstall(logger, &mut observer)?;
    if !opts.quiet {
        printer.print_success(&format!("{SOFTWARE_NAME} uninstalled completely!"));
        printer.print_info(&format!(
            "Note: Config ({CONFIG_DIR}) and data ({STATE_DIR}) were preserved"
        ));
        printer
            .print_info("Use 'purge' or 'uninstall --purge' to remove all configuration and data");
    }
    Ok(true)
}

// =============================================================================
/// Executes `purge` after `[y/N]` confirmation unless `force` is set.
///
/// # Errors
/// Returns [`AppError`] if root elevation, confirmation, or purge fails.
pub fn run_purge(
    force: bool,
    opts: SetupRunOpts,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    if !is_root() {
        delegate_or_reject_root("purge", force, false, opts, printer)?;
        return Ok(true);
    }
    if !opts.quiet {
        printer.print_header(&format!("Purging {SOFTWARE_NAME}"));
        printer.print_warning("This will remove ALL configuration and data!");
        printer.print_warning(&format!("Data directory: {STATE_DIR}"));
    }
    if !confirm_destructive(
        "Are you ABSOLUTELY sure? This cannot be undone!",
        "Purge cancelled",
        force,
        opts.quiet,
        printer,
    )? {
        return Ok(false);
    }
    let mut observer = |ev| {
        if !opts.quiet {
            printer.render_setup_event(&ev);
        }
    };
    installer::purge(logger, &mut observer)?;
    if !opts.quiet {
        printer.print_success(&format!(
            "{SOFTWARE_NAME} purged completely! All data removed."
        ));
    }
    Ok(true)
}

// =============================================================================
/// Enforces a `[y/N]` prompt for destructive operations unless `force` is set.
///
/// # Errors
/// Returns [`AppError::Usage`] if non-interactive without `--force` or [`AppError::Io`].
fn confirm_destructive(
    question: &str,
    cancel_msg: &str,
    force: bool,
    quiet: bool,
    printer: ConsolePrinter,
) -> AppResult<bool> {
    if force {
        return Ok(true);
    }
    if !stdin_is_interactive() {
        return Err(AppError::Usage(
            "Destructive setup action requires interactive confirmation or '--force'".into(),
        ));
    }
    eprintln!();
    match prompt_confirm(question, false) {
        Ok(true) => Ok(true),
        Ok(false) | Err(AppError::Cancelled) => {
            if !quiet {
                printer.print_info(cancel_msg);
            }
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

// =============================================================================
/// Re-invokes the current binary under `sudo` when launched from an unprivileged TTY.
///
/// # Errors
/// Returns [`AppError::Auth`] on non-TTY or [`AppError::Setup`] if `sudo` fails.
fn delegate_or_reject_root(
    subcommand: &str,
    force: bool,
    skip_deps: bool,
    opts: SetupRunOpts,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if !stdin_is_interactive() {
        return Err(AppError::Auth(format!(
            "Root privileges required (run: sudo ./setup.sh {subcommand})"
        )));
    }
    let exe = std::env::current_exe()
        .map_err(|err| AppError::io("Failed resolving current binary path for sudo", err))?;
    let exe_str = exe
        .to_str()
        .ok_or_else(|| AppError::Setup("Current executable path is not valid UTF-8".into()))?;

    if !opts.quiet {
        printer.print_info("Elevating privileges via sudo...");
    }
    let mut args = vec!["--", exe_str, "setup", subcommand];
    if force {
        args.push("--force");
    }
    if skip_deps {
        args.push("--no-deps");
    }
    if opts.quiet {
        args.push("--quiet");
    }
    if opts.debug {
        args.push("--debug");
    }
    let timeout = Duration::from_secs(ELEVATED_SETUP_TIMEOUT_SECS.max(SUDO_TIMEOUT_SECS));
    if !run_command_inherited("sudo", &args, timeout)? {
        return Err(AppError::Setup(format!(
            "Elevated 'setup {subcommand}' did not complete successfully"
        )));
    }
    Ok(())
}
