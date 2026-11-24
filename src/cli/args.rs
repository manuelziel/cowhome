//! Zero-bloat POSIX / `clig.dev` command-line argument parser and help output.
//!
//! Parses runtime subcommands (`daemon`, `start`, `stop`, `restart`, `status`, `test`,
//! `diagnostics`, `config`, `validate`, `export`, `import`, `setup`, `help`, `version`),
//! supports both `--flag value` and `--flag=value`, and delegates `setup` to [`super::setup_args`].

use std::path::PathBuf;

use crate::cli::import_export::ConfigFormat;
use crate::cli::output::write_stdout_line;
use crate::cli::setup_args::{SetupAction, parse_setup_invocation};
use crate::core::constants::{
    app_meta::{NAME, SOFTWARE_NAME, VERSION},
    diagnostics_defaults::{DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT},
    fhs_paths::SYSTEMD_UNIT_NAME,
};
use crate::core::diagnostics::DiagnosticsQuery;
use crate::core::error::{AppError, AppResult};

// =============================================================================
/// Parsed CLI invocation combining the target subcommand with global flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCli {
    /// Selected subcommand or TUI entrypoint.
    pub command: CliCommand,
    /// Override log threshold to `DEBUG` (`--debug`).
    pub debug: bool,
    /// Suppress non-error status output (`--quiet` / `-q`).
    pub quiet: bool,
}

// =============================================================================
/// Top-level parsed CLI command dispatched by `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    /// Launch interactive runtime TUI menu (no arguments or `--interactive` / `-i`).
    InteractiveMenu,
    /// Run foreground service loop (`daemon` / `--daemon` / `-d`, `systemd` `ExecStart=`).
    Daemon {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Start `<name>.service` via `systemctl`.
    Start,
    /// Stop `<name>.service` via `systemctl`.
    Stop,
    /// Restart `<name>.service` via `systemctl`.
    Restart,
    /// Display configuration, service, storage, and device status (`status` / `--info`).
    Status {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Probe live FritzBox `TR-064` & `AHA` credentials without daemon (`test`).
    Test {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Emit sanitized Universal Diagnostics Schema v1.0 JSON to `stdout`.
    Diagnostics {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
        /// Optional query filter parameters (`--range`, `--limit`, `--device`).
        query: DiagnosticsQuery,
    },
    /// Open interactive configuration & sysadmin editor (`config`).
    Config {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Strictly validate TOML syntax, schema, security bounds, and symlinks.
    Validate {
        /// Optional explicit configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Export CowHome backup (`JSON`, `YAML`) or `config.toml` (`TOML`).
    Export {
        /// Serialization format (`json`, `yaml`, `toml`).
        format: ConfigFormat,
        /// Optional output file path (`--output <path>` / `-o <path>`).
        output: Option<PathBuf>,
        /// Allow overwriting an existing output file (`--overwrite`).
        overwrite: bool,
        /// Optional explicit source configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// Validate and atomically import a CowHome backup or `config.toml`.
    Import {
        /// Input backup file path.
        source: PathBuf,
        /// Allow overwriting existing entries / target `config.toml` (`--overwrite`).
        overwrite: bool,
        /// Optional explicit destination configuration file (`--file <path>`).
        config_file: Option<PathBuf>,
    },
    /// System installation, update, uninstall, purge, requirements, or setup status.
    Setup(SetupAction),
    /// Print general CLI usage help.
    Help,
    /// Print binary name and semantic version to `stdout`.
    Version,
}

// =============================================================================
/// Parses command-line arguments (excluding `argv[0]`) into a [`ParsedCli`].
///
/// # Errors
/// Returns [`AppError::Usage`] on unknown subcommands, unknown flags, or missing values.
pub fn parse_cli_args(args: &[String]) -> AppResult<ParsedCli> {
    if args.first().map(String::as_str) == Some("setup") {
        let setup = parse_setup_invocation(&args[1..])?;
        return Ok(ParsedCli {
            command: CliCommand::Setup(setup.action),
            debug: setup.debug,
            quiet: setup.quiet,
        });
    }
    let normalized = expand_inline_equals_flags(args);
    let (mut debug, mut quiet, mut interactive) = (false, false, false);
    let mut filtered = Vec::with_capacity(normalized.len());

    for token in normalized {
        match token.as_str() {
            "--debug" => debug = true,
            "--quiet" | "-q" => quiet = true,
            "--interactive" | "-i" => interactive = true,
            _ => filtered.push(token),
        }
    }
    let command = parse_filtered_command(&filtered, interactive)?;
    Ok(ParsedCli {
        command,
        debug,
        quiet,
    })
}

// =============================================================================
/// Splits `--flag=value` (and `-f=value` / `-o=value`) tokens into `[flag, value]` pairs.
fn expand_inline_equals_flags(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    for token in args {
        if let Some((flag, val)) = token.split_once('=')
            && matches!(
                flag,
                "--file"
                    | "-f"
                    | "--format"
                    | "--output"
                    | "-o"
                    | "--range"
                    | "--limit"
                    | "--device"
            )
        {
            out.push(flag.to_owned());
            out.push(val.to_owned());
            continue;
        }
        out.push(token.clone());
    }
    out
}

// =============================================================================
/// Dispatches the primary runtime subcommand token from `filtered` arguments.
///
/// # Errors
/// Returns [`AppError::Usage`] on conflicting `--interactive` or unknown options.
fn parse_filtered_command(filtered: &[String], interactive: bool) -> AppResult<CliCommand> {
    let Some(first) = filtered.first().map(String::as_str) else {
        return Ok(CliCommand::InteractiveMenu);
    };
    if matches!(first, "help" | "--help" | "-h") {
        return Ok(CliCommand::Help);
    }
    if matches!(first, "version" | "--version" | "-V" | "-v") {
        return Ok(CliCommand::Version);
    }
    if interactive {
        return Err(AppError::Usage(format!(
            "'--interactive' cannot be combined with '{first}'"
        )));
    }
    let rest = &filtered[1..];
    match first {
        "start" => parse_flagless_cmd("start", rest, CliCommand::Start),
        "stop" => parse_flagless_cmd("stop", rest, CliCommand::Stop),
        "restart" => parse_flagless_cmd("restart", rest, CliCommand::Restart),
        "diagnostics" => parse_diagnostics_args(rest),
        "export" => parse_export_args(rest),
        "import" => parse_import_args(rest),
        other => parse_file_backed_command(other, rest),
    }
}

// =============================================================================
/// Dispatches subcommands accepting `--file <path>` (`daemon`, `status`, `test`, `config`, `validate`).
///
/// # Errors
/// Returns [`AppError::Usage`] on unknown commands or invalid flags.
fn parse_file_backed_command(first: &str, rest: &[String]) -> AppResult<CliCommand> {
    match first {
        "daemon" | "--daemon" | "-d" => {
            map_file_cmd("daemon", rest, |f| CliCommand::Daemon { config_file: f })
        }
        "status" | "--info" => {
            map_file_cmd("status", rest, |f| CliCommand::Status { config_file: f })
        }
        "test" => map_file_cmd("test", rest, |f| CliCommand::Test { config_file: f }),
        "config" => map_file_cmd("config", rest, |f| CliCommand::Config { config_file: f }),
        "validate" => map_file_cmd("validate", rest, |f| CliCommand::Validate {
            config_file: f,
        }),
        unknown => Err(AppError::Usage(format!(
            "Unknown command or option '{unknown}'"
        ))),
    }
}

// =============================================================================
/// Parses subcommands that accept only `-h` / `--help` (`start`, `stop`, `restart`).
///
/// # Errors
/// Returns [`AppError::Usage`] if unexpected arguments are passed.
fn parse_flagless_cmd(name: &str, args: &[String], cmd: CliCommand) -> AppResult<CliCommand> {
    if args.iter().any(|a| matches!(a.as_str(), "--help" | "-h")) {
        return Ok(CliCommand::Help);
    }
    if let Some(extra) = args.first() {
        return Err(AppError::Usage(format!(
            "Unexpected option '{extra}' for '{name}'"
        )));
    }
    Ok(cmd)
}

// =============================================================================
/// Parses optional `--file <path>` (`-f <path>`) or `--help` (`-h`) on runtime commands.
///
/// # Errors
/// Returns [`AppError::Usage`] on missing `--file` value or unexpected option.
fn map_file_cmd<F>(cmd_name: &str, args: &[String], ctor: F) -> AppResult<CliCommand>
where
    F: FnOnce(Option<PathBuf>) -> CliCommand,
{
    let mut config_file: Option<PathBuf> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => return Ok(CliCommand::Help),
            "--file" | "-f" => {
                config_file = Some(PathBuf::from(next_flag_value(args, idx, "--file")?));
                idx += 2;
            }
            other => {
                return Err(AppError::Usage(format!(
                    "Unexpected option '{other}' for '{cmd_name}'"
                )));
            }
        }
    }
    Ok(ctor(config_file))
}

// =============================================================================
/// Parses `diagnostics [--file <cfg>] [--range <...>] [--limit <1..200>] [--device <id>]`.
///
/// # Errors
/// Returns [`AppError::Usage`] on malformed options or out-of-bounds `--limit`.
fn parse_diagnostics_args(args: &[String]) -> AppResult<CliCommand> {
    let mut config_file: Option<PathBuf> = None;
    let mut query = DiagnosticsQuery::default();
    let mut idx = 0;

    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => return Ok(CliCommand::Help),
            flag @ ("--file" | "-f" | "--range" | "--limit" | "--device") => {
                let val = next_flag_value(args, idx, flag)?;
                apply_diagnostics_flag(flag, val, &mut config_file, &mut query)?;
                idx += 2;
            }
            other => {
                return Err(AppError::Usage(format!(
                    "Unexpected option '{other}' for 'diagnostics'"
                )));
            }
        }
    }
    Ok(CliCommand::Diagnostics { config_file, query })
}

// =============================================================================
/// Applies a single valued option onto `diagnostics` CLI state.
///
/// # Errors
/// Returns [`AppError::Usage`] if `--limit` is not an integer in `1..=200`.
fn apply_diagnostics_flag(
    flag: &str,
    val: &str,
    config_file: &mut Option<PathBuf>,
    query: &mut DiagnosticsQuery,
) -> AppResult<()> {
    match flag {
        "--file" | "-f" => *config_file = Some(PathBuf::from(val)),
        "--range" => query.range = Some(val.to_owned()),
        "--device" => query.device = Some(val.to_owned()),
        _ => {
            let parsed: usize = val.parse().map_err(|_| {
                AppError::Usage(format!(
                    "Invalid '--limit' integer '{val}' (expected 1..{MAX_LOG_LIMIT})"
                ))
            })?;
            if !(1..=MAX_LOG_LIMIT).contains(&parsed) {
                return Err(AppError::Usage(format!(
                    "'--limit' must be between 1 and {MAX_LOG_LIMIT}"
                )));
            }
            query.limit = Some(parsed);
        }
    }
    Ok(())
}

// =============================================================================
/// Parses `export [--format json|yaml|toml] [--output <file>] [--overwrite] [--file <cfg>]`.
///
/// # Errors
/// Returns [`AppError::Usage`] on unsupported formats, missing values, or unknown flags.
fn parse_export_args(args: &[String]) -> AppResult<CliCommand> {
    let mut format = ConfigFormat::default();
    let (mut output, mut config_file, mut overwrite, mut idx) = (None, None, false, 0);

    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => return Ok(CliCommand::Help),
            "--overwrite" => {
                overwrite = true;
                idx += 1;
            }
            flag @ ("--format" | "--output" | "-o" | "--file" | "-f") => {
                let val = next_flag_value(args, idx, flag)?;
                apply_export_value_flag(flag, val, &mut format, &mut output, &mut config_file)?;
                idx += 2;
            }
            other => {
                return Err(AppError::Usage(format!(
                    "Unexpected export option '{other}'"
                )));
            }
        }
    }
    Ok(CliCommand::Export {
        format,
        output,
        overwrite,
        config_file,
    })
}

// =============================================================================
/// Applies `--format`, `--output`, `--file`, or legacy `-f <json|yaml|toml>` on `export`.
///
/// # Errors
/// Returns [`AppError::Usage`] if `--format` receives an unrecognized format token.
fn apply_export_value_flag(
    flag: &str,
    val: &str,
    format: &mut ConfigFormat,
    output: &mut Option<PathBuf>,
    config_file: &mut Option<PathBuf>,
) -> AppResult<()> {
    match flag {
        "--format" => *format = ConfigFormat::parse_flag(val)?,
        "--output" | "-o" => *output = Some(PathBuf::from(val)),
        "-f" if matches!(
            val.to_ascii_lowercase().as_str(),
            "json" | "yaml" | "yml" | "toml"
        ) =>
        {
            *format = ConfigFormat::parse_flag(val)?;
        }
        _ => *config_file = Some(PathBuf::from(val)),
    }
    Ok(())
}

// =============================================================================
/// Parses `import <source_file> [--overwrite] [--file <target_cfg>]`.
///
/// # Errors
/// Returns [`AppError::Usage`] if `<source_file>` is missing or unknown flags are passed.
fn parse_import_args(args: &[String]) -> AppResult<CliCommand> {
    let (mut source, mut config_file, mut overwrite, mut idx) = (None, None, false, 0);

    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => return Ok(CliCommand::Help),
            "--overwrite" => {
                overwrite = true;
                idx += 1;
            }
            "--file" | "-f" => {
                config_file = Some(PathBuf::from(next_flag_value(args, idx, "--file")?));
                idx += 2;
            }
            arg if !arg.starts_with('-') && source.is_none() => {
                source = Some(PathBuf::from(arg));
                idx += 1;
            }
            other => {
                return Err(AppError::Usage(format!(
                    "Unexpected import option '{other}'"
                )));
            }
        }
    }
    let source = source
        .ok_or_else(|| AppError::Usage(format!("'{NAME} import <file>' requires a file path")))?;
    Ok(CliCommand::Import {
        source,
        overwrite,
        config_file,
    })
}

// =============================================================================
/// Fetches the value argument immediately following `flag_name` at `idx + 1`.
///
/// # Errors
/// Returns [`AppError::Usage`] if `idx + 1` is missing, empty, or starts with `'-'`.
fn next_flag_value<'a>(args: &'a [String], idx: usize, flag_name: &str) -> AppResult<&'a str> {
    match args.get(idx + 1) {
        Some(val) if !val.is_empty() && !val.starts_with('-') => Ok(val.as_str()),
        _ => Err(AppError::Usage(format!(
            "Option '{flag_name}' requires a value argument"
        ))),
    }
}

// =============================================================================
/// Writes `<name> <version>` to `stdout` without panicking on closed pipes (`EPIPE`).
pub fn print_version() {
    write_stdout_line(&format!("{NAME} {VERSION}"));
}

// =============================================================================
/// Writes general CLI usage documentation (`<= 78` cols) consistently to `stdout`.
pub fn print_help() {
    write_stdout_line(&format!(
        "\
═══ {SOFTWARE_NAME} v{VERSION} ═══

Usage: {NAME} [ACTION] [OPTIONS]

Actions:
  status, --info                Show service, storage, & live device status
  test [--file <path>]          Test FritzBox TR-064 & AHA credentials
  config [--file <path>]        Open interactive configuration & admin menu
  validate [--file <path>]      Validate configuration syntax & security
  export [--format F] [-o FILE] Export backup (json, yaml) or config (toml)
  import <file> [--overwrite]   Import CowHome backup or configuration
  diagnostics [options]         Emit sanitized JSON diagnostics (Schema v1.0)
  daemon [--file <path>]        Run foreground daemon (systemd target)
  start | stop | restart        Control {SYSTEMD_UNIT_NAME} via systemctl
  setup [ACTION]                Install, update, uninstall, or purge
  help, --help, -h              Show this help
  version, --version, -v        Show version information

Options:
  --file, -f <path>             Use explicit configuration file path
  --range <window>              Range: 24h|7d|30d|90d|180d|365d|1y|all
  --limit <1..{MAX_LOG_LIMIT}>              Diagnostics log line limit (default: {DEFAULT_LOG_LIMIT})
  --device <filter>             Diagnostics optional device filter
  --interactive, -i             Launch interactive runtime menu
  --quiet, -q                   Suppress non-error output
  --debug                       Enable debug output

Logs: sudo journalctl -u {SYSTEMD_UNIT_NAME} -f"
    ));
}
