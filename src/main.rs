//! Single-binary entry point and top-level CLI dispatcher for CowHome.
//!
//! Initializes monotonic process uptime tracking, dispatches subcommands across
//! async daemon, CLI status/test/diagnostics, TUI, and setup workflows, and maps
//! [`AppError`] variants to deterministic POSIX exit codes.

use std::path::Path;
use std::process;

use cowhome::cli::args::{parse_cli_args, print_help, print_version, CliCommand};
use cowhome::cli::config_menu::run_config_menu;
use cowhome::cli::fritz_test::run_fritz_connection_test;
use cowhome::cli::import_export::{run_export, run_import, ExportRequest};
use cowhome::cli::input::stdin_is_interactive;
use cowhome::cli::output::ConsolePrinter;
use cowhome::cli::runtime_menu::{elevate_subcommand_via_sudo, run_runtime_menu};
use cowhome::cli::setup_menu::execute_setup_action;
use cowhome::cli::status_view::show_runtime_status;
use cowhome::core::config::{
    load_runtime_config, load_runtime_config_lenient, validate_config_file, AppConfig, DebugConfig,
};
use cowhome::core::constants::app_meta::NAME;
use cowhome::core::constants::cli_style::EXIT_SUCCESS;
use cowhome::core::constants::fhs_paths::SYSTEMD_UNIT_NAME;
use cowhome::core::diagnostics::{init_process_clock, DiagnosticsQuery};
use cowhome::core::error::{AppError, AppResult};
use cowhome::core::logger::{LogLevel, Logger};
use cowhome::core::process::install_signal_handlers;
use cowhome::db::Database;
use cowhome::services::diagnostics::DiagnosticsService;
use cowhome::setup::systemd::control_service_unit;
use cowhome::web::daemon::run_daemon_supervisor;

// =============================================================================
/// Application entry point translating [`AppResult`] into POSIX process exit codes.
fn main() {
    init_process_clock();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(raw_args) {
        Ok(code) => process::exit(code),
        Err(AppError::Cancelled) => process::exit(EXIT_SUCCESS),
        Err(err) => {
            report_fatal_error(&err);
            process::exit(err.exit_code());
        }
    }
}

// =============================================================================
/// Routes parsed [`CliCommand`] variants to their corresponding workflows.
///
/// # Errors
/// Returns [`AppError`] on invalid CLI usage, configuration errors, or failed subcommands.
fn dispatch(raw_args: Vec<String>) -> AppResult<i32> {
    let parsed = parse_cli_args(&raw_args)?;
    if !matches!(parsed.command, CliCommand::Daemon { .. }) {
        install_signal_handlers();
    }
    let default_dbg = DebugConfig::default();
    let logger = build_default_logger(parsed.debug, &default_dbg);
    let printer = ConsolePrinter::new(logger.colors_enabled());

    match parsed.command {
        CliCommand::Help => {
            print_help();
            Ok(EXIT_SUCCESS)
        }
        CliCommand::Version => {
            print_version();
            Ok(EXIT_SUCCESS)
        }
        CliCommand::InteractiveMenu => run_interactive_or_help(parsed.debug),
        CliCommand::Daemon { config_file } => run_daemon(config_file.as_deref(), parsed.debug),
        CliCommand::Start => run_unit_verb("start", "Started", &logger, printer, parsed.quiet),
        CliCommand::Stop => run_unit_verb("stop", "Stopped", &logger, printer, parsed.quiet),
        CliCommand::Restart => {
            run_unit_verb("restart", "Restarted", &logger, printer, parsed.quiet)
        }
        other => dispatch_config_commands(other, parsed.debug, parsed.quiet, &logger, printer),
    }
}

// =============================================================================
/// Dispatches configuration, test, diagnostics, import/export, and setup commands.
///
/// # Errors
/// Returns [`AppError`] if the selected workflow fails.
fn dispatch_config_commands(
    cmd: CliCommand,
    debug: bool,
    quiet: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<i32> {
    match cmd {
        CliCommand::Status { config_file } => {
            run_status_cmd(config_file.as_deref(), debug, quiet)?;
        }
        CliCommand::Test { config_file } => run_test_cmd(config_file.as_deref(), debug)?,
        CliCommand::Diagnostics { config_file, query } => {
            run_diagnostics(config_file.as_deref(), &query, debug)?;
        }
        CliCommand::Config { config_file } => {
            run_config_cmd(config_file.as_deref(), debug, printer)?;
        }
        CliCommand::Validate { config_file } => {
            run_validate(config_file.as_deref(), debug, quiet)?;
        }
        CliCommand::Export {
            format,
            output,
            overwrite,
            config_file,
        } => {
            let req = ExportRequest {
                format,
                output_file: output.as_deref(),
                overwrite,
                explicit_cfg: config_file.as_deref(),
                quiet,
            };
            run_export(&req, logger, printer)?;
        }
        CliCommand::Import {
            source,
            overwrite,
            config_file,
        } => run_import(
            &source,
            overwrite,
            config_file.as_deref(),
            quiet,
            logger,
            printer,
        )?,
        CliCommand::Setup(action) => {
            execute_setup_action(action, quiet, debug, logger, printer)?;
        }
        _ => {}
    }
    Ok(EXIT_SUCCESS)
}

// =============================================================================
/// Loads runtime config leniently (non-root safe) and renders `cowhome status`.
///
/// # Errors
/// Returns [`AppError`] if an explicit `--file` is missing or TOML syntax is invalid.
fn run_status_cmd(explicit_file: Option<&Path>, debug: bool, quiet: bool) -> AppResult<()> {
    let (cfg, path, cfg_locked) = load_runtime_config_lenient(explicit_file)?;
    if quiet {
        return Ok(());
    }
    let (logger, printer) = build_runtime_context(&cfg, debug, !cfg_locked);
    show_runtime_status(&cfg, &path, cfg_locked, &logger, printer);
    Ok(())
}

// =============================================================================
/// Loads runtime config and executes `cowhome test` (`TR-064` & `AHA` connection test).
///
/// # Errors
/// Returns [`AppError`] if credentials are missing or FritzBox probing fails.
fn run_test_cmd(explicit_file: Option<&Path>, debug: bool) -> AppResult<()> {
    let (cfg, path) = load_runtime_config(explicit_file)?;
    let (logger, printer) = build_runtime_context(&cfg, debug, true);
    run_fritz_connection_test(&cfg, &path, &logger, printer)
}

// =============================================================================
/// Loads runtime config and opens `cowhome config` (auto-elevating via `sudo` on `0750` `/etc`).
///
/// # Errors
/// Returns [`AppError`] if loading configuration, `sudo` elevation, or TUI editing fails.
fn run_config_cmd(
    explicit_file: Option<&Path>,
    debug: bool,
    printer: ConsolePrinter,
) -> AppResult<()> {
    match load_runtime_config(explicit_file) {
        Ok((cfg, path)) => {
            let (logger, cfg_printer) = build_runtime_context(&cfg, debug, true);
            run_config_menu(&cfg, &path, &logger, cfg_printer)
        }
        Err(AppError::Auth(_)) if stdin_is_interactive() => {
            elevate_subcommand_via_sudo("config", explicit_file, debug, printer)
        }
        Err(err) => Err(err),
    }
}

// =============================================================================
/// Launches the interactive Runtime TUI on TTYs or prints usage help when piped.
///
/// # Errors
/// Returns [`AppError`] if interactive menu execution fails.
fn run_interactive_or_help(debug: bool) -> AppResult<i32> {
    if stdin_is_interactive() {
        run_runtime_menu(debug)?;
    } else {
        print_help();
    }
    Ok(EXIT_SUCCESS)
}

// =============================================================================
/// Starts the full async `cowhome daemon` supervisor (`axum`, workers, `RulesEngine`).
///
/// # Errors
/// Returns [`AppError`] if config loading, database initialization, or port binding fails.
fn run_daemon(explicit_file: Option<&Path>, debug_override: bool) -> AppResult<i32> {
    let (cfg, _path) = load_runtime_config(explicit_file)?;
    let (logger, _printer) = build_runtime_context(&cfg, debug_override, true);
    run_daemon_supervisor(cfg, explicit_file, debug_override, logger)
}

// =============================================================================
/// Executes `systemctl <start|stop|restart>` and emits confirmation when not `--quiet`.
///
/// # Errors
/// Returns [`AppError`] if `systemctl` fails or unit is missing.
fn run_unit_verb(
    verb: &str,
    past_tense: &str,
    logger: &Logger,
    printer: ConsolePrinter,
    quiet: bool,
) -> AppResult<i32> {
    control_service_unit(verb, logger)?;
    if !quiet {
        printer.print_success(&format!("{past_tense} {SYSTEMD_UNIT_NAME}"));
    }
    Ok(EXIT_SUCCESS)
}

// =============================================================================
/// Emits a pretty-printed Universal Diagnostics Schema v1.0 JSON report to `stdout`.
///
/// # Errors
/// Returns [`AppError`] on invalid query range, config error, or JSON serialization failure.
fn run_diagnostics(
    explicit_file: Option<&Path>,
    query: &DiagnosticsQuery,
    debug_override: bool,
) -> AppResult<()> {
    let (cfg, _path, cfg_locked) = load_runtime_config_lenient(explicit_file)?;
    let (logger, printer) = build_runtime_context(&cfg, debug_override, !cfg_locked);
    let db_path = cfg.resolved_database_path();
    let db_opt = if db_path.exists() {
        match Database::open(&db_path, &cfg.weather.timezone, logger.clone()) {
            Ok(db) => Some(db),
            Err(err) => {
                logger.warning(&format!(
                    "Diagnostics proceeding without SQLite database '{}': {err}",
                    db_path.display()
                ));
                None
            }
        }
    } else {
        None
    };

    let collector = DiagnosticsService::new(cfg, db_opt, None, None, None, None, logger);
    let report = collector.collect_report(query)?;
    let encoded = serde_json::to_string_pretty(&report)
        .map_err(|err| AppError::Service(format!("Diagnostics serialization failed: {err}")))?;
    printer.stdout_line(&encoded);
    Ok(())
}

// =============================================================================
/// Validates a configuration file strictly and reports status on `stderr`.
///
/// # Errors
/// Returns [`AppError`] if the configuration file is missing, unreadable, or invalid.
fn run_validate(explicit_file: Option<&Path>, debug_override: bool, quiet: bool) -> AppResult<()> {
    let (cfg, path) = validate_config_file(explicit_file)?;
    if !quiet {
        let (_logger, printer) = build_runtime_context(&cfg, debug_override, true);
        printer.print_success(&format!("Configuration '{}' is valid", path.display()));
    }
    Ok(())
}

// =============================================================================
/// Builds the default [`Logger`] before runtime configuration is loaded.
fn build_default_logger(debug_override: bool, default_dbg: &DebugConfig) -> Logger {
    let level = if debug_override {
        LogLevel::Debug
    } else {
        default_dbg.level
    };
    Logger::new(level, default_dbg.console_colors)
}

// =============================================================================
/// Builds the dependency-injected [`Logger`] (with secret redaction) and [`ConsolePrinter`].
fn build_runtime_context(
    cfg: &AppConfig,
    debug_override: bool,
    redact_secrets: bool,
) -> (Logger, ConsolePrinter) {
    let level = if debug_override {
        LogLevel::Debug
    } else {
        cfg.debug.level
    };
    let logger = Logger::new(level, cfg.debug.console_colors);
    if redact_secrets {
        logger.reconfigure(
            level,
            cfg.debug.console_colors,
            &[&cfg.api.api_key, &cfg.fritzbox.password],
        );
    }
    let printer = ConsolePrinter::new(logger.colors_enabled());
    (logger, printer)
}

// =============================================================================
/// Logs unrecoverable errors to `journald` and emits a formatted error line on `stderr`.
fn report_fatal_error(err: &AppError) {
    let default_dbg = DebugConfig::default();
    let logger = Logger::new(default_dbg.level, default_dbg.console_colors);
    let printer = ConsolePrinter::new(logger.colors_enabled());

    match err {
        AppError::Io { .. } | AppError::Setup(_) | AppError::Security(_) => {
            logger.critical(&err.to_string());
        }
        _ => logger.error(&err.to_string()),
    }
    printer.print_error(&err.to_string());
    if matches!(err, AppError::Usage(_)) {
        eprintln!("Try '{NAME} --help' for usage information.");
    }
}
