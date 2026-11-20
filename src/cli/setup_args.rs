//! Setup subcommand model (`SetupAction`) and CLI flag parser (`<app> setup ...`).
//!
//! Leaf parser inside `src/cli/` depending solely on `crate::core::error`, keeping
//! [`crate::cli::args`] and [`crate::cli::setup_menu`] compact with zero sibling cycles.

use crate::core::error::{AppError, AppResult};

// =============================================================================
/// Parsed `<app> setup ...` invocation combining [`SetupAction`] with global flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSetupArgs {
    /// Selected setup subcommand.
    pub action: SetupAction,
    /// Override log threshold to `DEBUG` (`--debug`).
    pub debug: bool,
    /// Suppress non-error status output (`--quiet` / `-q`).
    pub quiet: bool,
}

// =============================================================================
/// Parsed sub-actions and flags supported under `<name> setup` / `./setup.sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAction {
    /// Launch the interactive context-sensitive setup menu.
    Interactive {
        /// Skip dependency installation (`--no-deps`).
        skip_deps: bool,
    },
    /// Install binary, default config, systemd unit, and optional desktop starter.
    Install {
        /// Overwrite an existing installation without aborting (`--force` / `-f`).
        force: bool,
        /// Skip dependency installation (`--no-deps`).
        skip_deps: bool,
    },
    /// Update binary and units while preserving `config.toml` and `/var/lib/<name>`.
    Update {
        /// Skip dependency installation (`--no-deps`).
        skip_deps: bool,
    },
    /// Reinstall all system files while preserving `config.toml` and `/var/lib/<name>`.
    Reinstall {
        /// Skip dependency installation (`--no-deps`).
        skip_deps: bool,
    },
    /// Remove binary and units while keeping `/etc/<name>` and `/var/lib/<name>`.
    Uninstall {
        /// Skip interactive `[y/N]` confirmation (`--force` / `-f` / `-y`).
        force: bool,
    },
    /// Remove all installed files including `/etc/<name>` and `/var/lib/<name>`.
    Purge {
        /// Skip interactive `[y/N]` confirmation (`--force` / `-f` / `-y`).
        force: bool,
    },
    /// Show installation status (`status` / `--info`).
    Status {
        /// Skip dependency check (`--no-deps`).
        skip_deps: bool,
    },
    /// Show OS and package requirements (`requirements`).
    Requirements,
    /// Show multi-line software version block (`version` / `--version` / `-v`).
    Version,
    /// Print setup subcommand usage help (`help` / `--help` / `-h`).
    Help,
}

// =============================================================================
/// Parses `<name> setup [ACTION] [OPTIONS]` with full `02_linux/_project_template` parity.
///
/// # Errors
/// Returns [`AppError::Usage`] on conflicting actions or unrecognized setup flags.
pub fn parse_setup_invocation(args: &[String]) -> AppResult<ParsedSetupArgs> {
    let (mut debug, mut quiet, mut force, mut skip_deps, mut purge_flag, mut help_req) =
        (false, false, false, false, false, false);
    let mut slot: Option<&str> = None;

    for token in args {
        match token.as_str() {
            "--debug" => debug = true,
            "--quiet" | "-q" => quiet = true,
            "--force" | "-f" | "-y" => force = true,
            "--no-deps" => skip_deps = true,
            "--interactive" | "-i" => {}
            "--purge" => purge_flag = true,
            "help" | "--help" | "-h" => help_req = true,
            "install" => set_action(&mut slot, "install", token)?,
            "update" | "--upgrade" => set_action(&mut slot, "update", token)?,
            "reinstall" | "--reinstall" => set_action(&mut slot, "reinstall", token)?,
            "uninstall" | "--remove" => set_action(&mut slot, "uninstall", token)?,
            "purge" => set_action(&mut slot, "purge", token)?,
            "status" | "--info" => set_action(&mut slot, "status", token)?,
            "requirements" => set_action(&mut slot, "requirements", token)?,
            "version" | "--version" | "-v" | "-V" => set_action(&mut slot, "version", token)?,
            other => return Err(AppError::Usage(format!("Unknown setup option '{other}'"))),
        }
    }
    let action = if help_req {
        SetupAction::Help
    } else {
        build_setup_action(slot, force, skip_deps, purge_flag)
    };
    Ok(ParsedSetupArgs {
        action,
        debug,
        quiet,
    })
}

// =============================================================================
/// Ensures at most one primary setup action is specified per invocation.
///
/// # Errors
/// Returns [`AppError::Usage`] if a setup action was already set.
fn set_action<'a>(slot: &mut Option<&'a str>, canonical: &'a str, raw: &str) -> AppResult<()> {
    if let Some(prev) = *slot {
        return Err(AppError::Usage(format!(
            "Multiple setup actions specified: '{prev}' and '{raw}'"
        )));
    }
    *slot = Some(canonical);
    Ok(())
}

// =============================================================================
/// Maps parsed setup action tokens and flags into a typed [`SetupAction`].
fn build_setup_action(
    action_key: Option<&str>,
    force: bool,
    skip_deps: bool,
    purge_flag: bool,
) -> SetupAction {
    match action_key {
        None if purge_flag => SetupAction::Purge { force },
        None => SetupAction::Interactive { skip_deps },
        Some("install") => SetupAction::Install { force, skip_deps },
        Some("update") => SetupAction::Update { skip_deps },
        Some("reinstall") => SetupAction::Reinstall { skip_deps },
        Some("uninstall") if purge_flag => SetupAction::Purge { force },
        Some("uninstall") => SetupAction::Uninstall { force },
        Some("purge") => SetupAction::Purge { force },
        Some("status") => SetupAction::Status { skip_deps },
        Some("requirements") => SetupAction::Requirements,
        Some("version") => SetupAction::Version,
        Some("help") => SetupAction::Help,
        Some(_) => SetupAction::Interactive { skip_deps },
    }
}
