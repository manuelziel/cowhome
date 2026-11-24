//! Backup and configuration export/import (`cowhome export` & `cowhome import`).
//!
//! Supports both full CowHome SQLite entity backups (`YAML` / `JSON`: groups, schedules,
//! custom names, & `EnWG § 40` tariffs) and `config.toml` (`TOML` / `AppConfig`), with
//! `CWE-59` symlink checks, `~` expansion, `0600` permissions, and `SUDO_USER` ownership.

use std::path::{Path, PathBuf};

use chrono::Local;
use serde_json::Value;

use crate::cli::backup_db::{build_db_backup_value, import_db_backup_value, is_db_backup_payload};
use crate::cli::input::{prompt_confirm, prompt_line};
use crate::cli::output::ConsolePrinter;
use crate::cli::yaml_codec::{emit_yaml_document, parse_yaml_document};
use crate::core::config::{
    AppConfig, format_config_toml, load_runtime_config, parse_strict_config_toml,
    resolve_config_path, save_config,
};
use crate::core::constants::{
    app_meta::SOFTWARE_NAME,
    cli_style::{ANSI_CYAN, ANSI_DIM},
    service_defaults::PRIVATE_FILE_MODE,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::security::{
    apply_caller_ownership_if_sudo, atomic_write_file, ensure_not_symlink, expand_tilde,
    read_file_nofollow,
};

/// Top-level keys identifying an `AppConfig` (`config.toml`) document in JSON or YAML.
const APP_CONFIG_KEYS: &[&str] = &[
    "debug",
    "server",
    "api",
    "rate_limit",
    "fritzbox",
    "daemon",
    "database",
    "weather",
    "automation",
];

// =============================================================================
/// Serialization format supported by `export` and `import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigFormat {
    /// Pretty-printed `JSON` (`RFC 8259`).
    #[default]
    Json,
    /// Two-space indented `YAML 1.2` block mapping.
    Yaml,
    /// `TOML v1.0` configuration document (`config.toml`).
    Toml,
}

impl ConfigFormat {
    /// Parses `--format <json|yaml|toml>` case-insensitively.
    ///
    /// # Errors
    /// Returns [`AppError::Usage`] if `raw` is not a supported format name.
    pub fn parse_flag(raw: &str) -> AppResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            "yaml" | "yml" => Ok(Self::Yaml),
            "toml" => Ok(Self::Toml),
            other => Err(AppError::Usage(format!(
                "Unsupported export format '{other}' (expected 'json', 'yaml', or 'toml')"
            ))),
        }
    }

    /// Detects the document format from file extension or leading content token.
    #[must_use]
    pub fn detect(path: &Path, content: &str) -> Self {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            match ext.to_ascii_lowercase().as_str() {
                "json" => return Self::Json,
                "yaml" | "yml" => return Self::Yaml,
                "toml" => return Self::Toml,
                _ => {}
            }
        }
        let trimmed = content.trim_start();
        if trimmed.starts_with('{') {
            Self::Json
        } else if trimmed
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .any(|l| l.trim_start().starts_with('['))
        {
            Self::Toml
        } else {
            Self::Yaml
        }
    }
}

// =============================================================================
/// Bundled CLI parameters for `cowhome export`.
#[derive(Debug, Clone)]
pub struct ExportRequest<'a> {
    /// Selected serialization format (`Json`, `Yaml`, or `Toml`).
    pub format: ConfigFormat,
    /// Optional destination file (`None` writes pure data to `stdout`).
    pub output_file: Option<&'a Path>,
    /// Allow replacing an existing `output_file`.
    pub overwrite: bool,
    /// Optional explicit `config.toml` path (`--file`).
    pub explicit_cfg: Option<&'a Path>,
    /// Suppress non-error confirmation messages on `stderr`.
    pub quiet: bool,
}

// =============================================================================
/// Exports CowHome backup (`JSON`/`YAML`) or `config.toml` (`TOML`) to `stdout` or `0600` file.
///
/// # Errors
/// Returns [`AppError`] on config/DB read failure, existing file without `--overwrite`, or I/O.
pub fn run_export(
    req: &ExportRequest<'_>,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let (cfg, _) = load_runtime_config(req.explicit_cfg)?;
    let encoded = render_export_payload(&cfg, req.format, logger)?;

    let Some(raw_dest) = req.output_file else {
        printer.stdout_line(encoded.trim_end());
        return Ok(());
    };

    let dest = resolve_user_path(raw_dest)?;
    write_private_export_file(&dest, &encoded, req.overwrite)?;
    logger.info(&format!("Exported configuration to '{}'", dest.display()));
    if !req.quiet {
        printer.print_success(&format!(
            "Exported configuration to '{}' (mode 0600)",
            dest.display()
        ));
    }
    Ok(())
}

// =============================================================================
/// Imports a CowHome SQLite entity backup (`JSON`/`YAML`) or `AppConfig` (`TOML`/`JSON`/`YAML`).
///
/// # Errors
/// Returns [`AppError`] on missing/invalid input, overwrite refusal, or persistence failure.
pub fn run_import(
    source_file: &Path,
    overwrite: bool,
    target_cfg: Option<&Path>,
    quiet: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let resolved_src = resolve_user_path(source_file)?;
    let raw = read_file_nofollow(&resolved_src)?;
    let fmt = ConfigFormat::detect(&resolved_src, &raw);
    if fmt == ConfigFormat::Toml {
        return import_toml_config(&raw, &resolved_src, overwrite, target_cfg, quiet, printer);
    }

    let doc = parse_structured_doc(&raw, fmt, &resolved_src)?;
    if is_db_backup_payload(&doc) {
        let (cfg, _) = load_runtime_config(target_cfg)?;
        import_db_backup_value(&doc, overwrite, &cfg, logger)?;
        logger.info(&format!(
            "Imported backup from '{}'",
            resolved_src.display()
        ));
        if !quiet {
            printer.print_success(&format!(
                "Imported groups, schedules, names, & tariffs from '{}'",
                resolved_src.display()
            ));
        }
        return Ok(());
    }
    if is_app_config_payload(&doc) {
        return import_structured_app_config(
            &doc,
            &resolved_src,
            overwrite,
            target_cfg,
            quiet,
            printer,
        );
    }
    Err(AppError::Validation(format!(
        "Unrecognized backup schema in '{}' (expected CowHome backup or AppConfig keys)",
        resolved_src.display()
    )))
}

// =============================================================================
/// Interactive TUI export workflow (`cowhome config` -> `Export Backup`).
///
/// # Errors
/// Returns [`AppError`] on unrecoverable terminal stream failure.
pub fn run_interactive_export(
    config: &AppConfig,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    printer.print_subsection("Export Backup");
    let ts = Local::now().format("%Y%m%d_%H%M%S");
    let default_name = format!("cowhome-export_{ts}.yaml");
    let default_path =
        expand_tilde(&format!("~/{default_name}")).unwrap_or_else(|_| PathBuf::from(&default_name));
    eprintln!(
        "  {} {}",
        printer.colorize("Default location:", ANSI_DIM),
        printer.colorize(&default_path.display().to_string(), ANSI_CYAN)
    );
    let raw = match prompt_line(
        "Output file path",
        Some(&default_path.display().to_string()),
    ) {
        Ok(v) => v,
        Err(AppError::Cancelled) => return Ok(()),
        Err(err) => return Err(err),
    };
    let dest = resolve_user_path(Path::new(&raw))?;
    let overwrite = if dest.exists() {
        prompt_confirm("File already exists. Overwrite?", false).unwrap_or(false)
    } else {
        true
    };
    if dest.exists() && !overwrite {
        printer.print_info("Export cancelled.");
        return Ok(());
    }
    let fmt = detect_interactive_export_format(&dest)?;
    let encoded = render_export_payload(config, fmt, logger)?;
    write_private_export_file(&dest, &encoded, true)?;
    printer.print_success(&format!("Backup exported to '{}'", dest.display()));
    Ok(())
}

// =============================================================================
/// Interactive TUI import workflow (`cowhome config` -> `Import Backup`).
///
/// # Errors
/// Returns [`AppError`] on unrecoverable terminal stream failure.
pub fn run_interactive_import(
    explicit_cfg: Option<&Path>,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    printer.print_subsection("Import Backup");
    let raw = match prompt_line("Enter backup file path to import", None) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => {
            printer.print_error("No file specified.");
            return Ok(());
        }
        Err(AppError::Cancelled) => return Ok(()),
        Err(err) => return Err(err),
    };
    let overwrite = match prompt_confirm("Overwrite existing entries?", false) {
        Ok(v) => v,
        Err(AppError::Cancelled) => return Ok(()),
        Err(err) => return Err(err),
    };
    run_import(
        Path::new(&raw),
        overwrite,
        explicit_cfg,
        false,
        logger,
        printer,
    )
}

// =============================================================================
/// Determines `ConfigFormat` from `dest` extension or prompts (`yaml`/`json`/`toml`).
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
fn detect_interactive_export_format(dest: &Path) -> AppResult<ConfigFormat> {
    if let Some(ext) = dest.extension().and_then(|s| s.to_str())
        && let Ok(parsed) = ConfigFormat::parse_flag(ext)
    {
        return Ok(parsed);
    }
    let raw = match prompt_line("Format (yaml/json/toml)", Some("yaml")) {
        Ok(v) => v,
        Err(AppError::Cancelled) => String::from("yaml"),
        Err(err) => return Err(err),
    };
    Ok(ConfigFormat::parse_flag(&raw).unwrap_or(ConfigFormat::Yaml))
}

// =============================================================================
/// Serializes either the SQLite domain backup (`Json`/`Yaml`) or `AppConfig` (`Toml`).
///
/// # Errors
/// Returns [`AppError`] if querying SQLite or serializing JSON fails.
fn render_export_payload(
    cfg: &AppConfig,
    format: ConfigFormat,
    logger: &Logger,
) -> AppResult<String> {
    match format {
        ConfigFormat::Toml => Ok(format_config_toml(cfg)),
        ConfigFormat::Json => {
            let val = build_db_backup_value(cfg, logger)?;
            let mut s = serde_json::to_string_pretty(&val)
                .map_err(|e| AppError::Config(format!("JSON serialization failed: {e}")))?;
            s.push('\n');
            Ok(s)
        }
        ConfigFormat::Yaml => {
            let val = build_db_backup_value(cfg, logger)?;
            Ok(emit_yaml_document(
                &val,
                &format!("{SOFTWARE_NAME} configuration backup"),
            ))
        }
    }
}

// =============================================================================
/// Atomically writes `encoded` to `dest` (`0600`) and hands ownership back to `SUDO_USER`.
///
/// # Errors
/// Returns [`AppError`] on symlink detection, overwrite refusal, or filesystem error.
fn write_private_export_file(dest: &Path, encoded: &str, overwrite: bool) -> AppResult<()> {
    ensure_not_symlink(dest)?;
    if dest.exists() && !overwrite {
        return Err(AppError::Validation(format!(
            "Output file '{}' already exists (pass --overwrite to replace)",
            dest.display()
        )));
    }
    atomic_write_file(dest, encoded.as_bytes(), PRIVATE_FILE_MODE)?;
    apply_caller_ownership_if_sudo(dest)
}

// =============================================================================
/// Parses `raw` as `JSON` or `YAML` into a generic [`Value`].
///
/// # Errors
/// Returns [`AppError::Config`] if `raw` is not valid JSON or YAML.
fn parse_structured_doc(raw: &str, fmt: ConfigFormat, path: &Path) -> AppResult<Value> {
    match fmt {
        ConfigFormat::Json => serde_json::from_str(raw)
            .map_err(|e| AppError::Config(format!("Invalid JSON in '{}': {e}", path.display()))),
        _ => parse_yaml_document(raw)
            .map_err(|e| AppError::Config(format!("Invalid YAML in '{}': {e}", path.display()))),
    }
}

// =============================================================================
/// Returns `true` if `val` contains `AppConfig` (`config.toml`) section keys.
fn is_app_config_payload(val: &Value) -> bool {
    val.as_object()
        .is_some_and(|obj| APP_CONFIG_KEYS.iter().any(|k| obj.contains_key(*k)))
}

// =============================================================================
/// Validates and imports a strict TOML document into `config.toml`.
///
/// # Errors
/// Returns [`AppError`] if TOML validation or atomic saving fails.
fn import_toml_config(
    raw: &str,
    src: &Path,
    overwrite: bool,
    target_cfg: Option<&Path>,
    quiet: bool,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let cfg = parse_strict_config_toml(raw, &src.display().to_string())?;
    persist_imported_app_config(&cfg, overwrite, target_cfg, quiet, printer)
}

// =============================================================================
/// Validates a JSON/YAML `AppConfig` object via strict schema rules and writes `config.toml`.
///
/// # Errors
/// Returns [`AppError`] if schema conversion, validation, or saving fails.
fn import_structured_app_config(
    doc: &Value,
    src: &Path,
    overwrite: bool,
    target_cfg: Option<&Path>,
    quiet: bool,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let toml_val: toml::Value = serde_json::from_value(doc.clone()).map_err(|e| {
        AppError::Validation(format!(
            "Invalid AppConfig value in '{}': {e}",
            src.display()
        ))
    })?;
    let serialized = toml::to_string(&toml_val).map_err(|e| {
        AppError::Validation(format!(
            "TOML conversion failed for '{}': {e}",
            src.display()
        ))
    })?;
    let cfg = parse_strict_config_toml(&serialized, &src.display().to_string())?;
    persist_imported_app_config(&cfg, overwrite, target_cfg, quiet, printer)
}

// =============================================================================
/// Writes a validated [`AppConfig`] to `target_cfg` (`config.toml`) respecting `overwrite`.
///
/// # Errors
/// Returns [`AppError`] if `dest` is a symlink, exists without `--overwrite`, or write fails.
fn persist_imported_app_config(
    cfg: &AppConfig,
    overwrite: bool,
    target_cfg: Option<&Path>,
    quiet: bool,
    printer: ConsolePrinter,
) -> AppResult<()> {
    let dest = resolve_config_path(target_cfg);
    ensure_not_symlink(&dest)?;
    if dest.exists() && !overwrite {
        return Err(AppError::Validation(format!(
            "Target configuration '{}' already exists (pass --overwrite to replace)",
            dest.display()
        )));
    }
    save_config(cfg, &dest)?;
    if !quiet {
        printer.print_success(&format!(
            "Imported and validated configuration into '{}'",
            dest.display()
        ));
    }
    Ok(())
}

// =============================================================================
/// Expands leading `~` / `~/...` against the invoking `SUDO_USER` or current user home.
///
/// # Errors
/// Returns [`AppError::Auth`] if `~` expansion is needed and `passwd` lookup fails.
fn resolve_user_path(raw_path: &Path) -> AppResult<PathBuf> {
    match raw_path.to_str() {
        Some(text) if text == "~" || text.starts_with("~/") => expand_tilde(text),
        _ => Ok(raw_path.to_path_buf()),
    }
}
