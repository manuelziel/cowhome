//! Status, system requirements, and version views for CowHome (`cowhome status`).
//!
//! Displays masked secrets (`••••••••abcd`), HTTP server & FritzBox parameters,
//! live `systemd` service health, SQLite database presence, and `statvfs` disk space.

use std::path::Path;

use crate::cli::output::{write_stdout_line, ConsolePrinter};
use crate::cli::status_live::render_live_cowhome_telemetry;
use crate::core::config::AppConfig;
use crate::core::constants::app_meta::{
    AUTHORS, LAST_MODIFIED, LICENSE, NAME, SOFTWARE_NAME, VERSION,
};
use crate::core::constants::cli_style::{
    ANSI_BLUE, ANSI_GREEN, ANSI_RED, ANSI_YELLOW, STATUS_LABEL_WIDTH, SYM_ERR, SYM_INFO, SYM_OK,
};
use crate::core::constants::diagnostics_defaults::DISK_WARN_PERCENT;
use crate::core::constants::fhs_paths::{
    BINARY_PATH, DESKTOP_ENTRY_PATH, RUN_DIR, STATE_DIR, SYSTEMD_UNIT_PATH,
};
use crate::core::constants::service_defaults::ENABLE_DESKTOP_ENTRY;
use crate::core::diagnostics::query_disk_space;
use crate::core::logger::Logger;
use crate::core::security::{probe_path_presence, PathPresence};
use crate::core::validation::{is_api_key_secure, mask_secret};
use crate::setup::deps::{read_os_pretty_name, REQUIRED_DEPENDENCIES};
use crate::setup::installer::{inspect_setup_status, SetupStatusReport};

// =============================================================================
/// Displays *Active Configuration*, *Service & Storage Health*, and live device telemetry.
pub fn show_runtime_status(
    config: &AppConfig,
    config_path: &Path,
    config_locked: bool,
    logger: &Logger,
    printer: ConsolePrinter,
) {
    logger.debug(&format!(
        "Rendering status view (log threshold {})",
        logger.min_level()
    ));
    render_active_config_section(config, config_path, config_locked, printer);
    render_service_and_storage_section(config, printer);
    if !config_locked {
        render_live_cowhome_telemetry(config, logger, printer);
    }
}

// =============================================================================
/// Prints *Active Configuration* with masked credentials (or `0750` permission hint).
fn render_active_config_section(
    config: &AppConfig,
    config_path: &Path,
    config_locked: bool,
    printer: ConsolePrinter,
) {
    printer.print_header("Active Configuration");
    if config_locked {
        print_kv("Config File:", &config_path.display().to_string());
        printer.print_info(&format!(
            "Protected (0750/0640) — run 'sudo {NAME} status' for full config & live devices"
        ));
        return;
    }
    let fb_user = if config.fritzbox.username.trim().is_empty() {
        "default"
    } else {
        config.fritzbox.username.as_str()
    };
    let vac_state = if config.automation.vacation_mode {
        "on"
    } else {
        "off"
    };
    print_kv("Config File:", &config_path.display().to_string());
    print_kv("Log Level:", config.debug.level.as_str());
    print_kv(
        "HTTP Server:",
        &format!(
            "{}:{} · {}",
            config.server.host, config.server.port, config.server.base_url
        ),
    );
    print_kv(
        "API Key:",
        &format_api_key_badge(&config.api.api_key, printer),
    );
    print_kv(
        "FritzBox:",
        &format!(
            "{} · User: {fb_user} · Pass: {} · Poll: {}s",
            config.fritzbox.host,
            mask_secret(&config.fritzbox.password),
            config.fritzbox.poll_interval
        ),
    );
    print_kv(
        "Weather:",
        &format!(
            "{:.4}, {:.4} · {}",
            config.weather.latitude, config.weather.longitude, config.weather.timezone
        ),
    );
    print_kv(
        "Automation:",
        &format!(
            "{}s cycle · Vacation: {vac_state}",
            config.automation.interval
        ),
    );
}

// =============================================================================
/// Renders masked valid API keys (`••••••••abcd`) or a warning badge for insecure placeholders.
#[must_use]
pub fn format_api_key_badge(api_key: &str, printer: ConsolePrinter) -> String {
    if is_api_key_secure(api_key) {
        mask_secret(api_key)
    } else if api_key.trim().is_empty() {
        printer.colorize(&format!("{SYM_ERR} (not set)"), ANSI_YELLOW)
    } else {
        printer.colorize(&format!("{SYM_ERR} insecure placeholder"), ANSI_YELLOW)
    }
}

// =============================================================================
/// Prints a 2-space indented key-value row aligned to `STATUS_LABEL_WIDTH`.
fn print_kv(label: &str, value: &str) {
    eprintln!("  {label:<STATUS_LABEL_WIDTH$} {value}");
}

// =============================================================================
/// Prints live `systemd` unit state, database path, and `statvfs` disk space telemetry.
fn render_service_and_storage_section(config: &AppConfig, printer: ConsolePrinter) {
    printer.print_header("Service & Storage Health");
    let report = inspect_setup_status(false);
    render_software_status_lines(&report, printer);
    render_service_status_line(&report, printer);

    let db_path = config.resolved_database_path();
    let db_state = match probe_path_presence(&db_path) {
        PathPresence::Accessible => "present",
        PathPresence::PermissionDenied => "present (0750 protected)",
        PathPresence::Missing => "not initialized",
    };
    printer.print_status_line(
        "Database",
        ANSI_BLUE,
        SYM_INFO,
        &format!("{} ({db_state})", db_path.display()),
    );

    let disk_probe = if Path::new(STATE_DIR).exists() {
        Path::new(STATE_DIR)
    } else {
        Path::new("/")
    };
    if let Some(disk) = query_disk_space(disk_probe) {
        let free_gb = disk.free_bytes as f64 / 1_073_741_824.0;
        let msg = format!("{free_gb:.2} GB free ({:.1}% used)", disk.used_percent);
        let (color, sym) = if disk.used_percent >= DISK_WARN_PERCENT {
            (ANSI_YELLOW, SYM_ERR)
        } else {
            (ANSI_GREEN, SYM_OK)
        };
        printer.print_status_line("Disk Space", color, sym, &msg);
    }
}

// =============================================================================
/// Renders the compact Setup TUI header status (`Software`, `Dependencies`, `Internet`).
pub fn show_setup_header_status(skip_deps: bool, printer: ConsolePrinter) {
    let report = inspect_setup_status(true);
    render_software_status_lines(&report, printer);
    render_dependencies_status_line(&report, skip_deps, printer);
    if report.internet_connected {
        printer.print_status_line("Internet", ANSI_GREEN, SYM_OK, "Connected");
    } else {
        printer.print_status_line("Internet", ANSI_YELLOW, SYM_ERR, "Not available");
    }
}

// =============================================================================
/// Renders the Runtime TUI header status (`Software`, `Dependencies`, `Service`, `Config`).
pub fn show_runtime_header_status(config_path: &Path, printer: ConsolePrinter) {
    let report = inspect_setup_status(false);
    render_software_status_lines(&report, printer);
    render_dependencies_status_line(&report, false, printer);
    render_service_status_line(&report, printer);

    eprintln!("\nConfiguration:");
    eprintln!("  Install path:  {BINARY_PATH}");
    eprintln!("  Config file:   {}", config_path.display());
}

// =============================================================================
/// Renders `Software: ✗ Not installed` or `Installed` / `Location` / `Size` / `Modified`.
fn render_software_status_lines(report: &SetupStatusReport, printer: ConsolePrinter) {
    let Some(info) = &report.installed_info else {
        printer.print_status_line("Software", ANSI_YELLOW, SYM_ERR, "Not installed");
        return;
    };
    printer.print_status_line(
        "Installed",
        ANSI_BLUE,
        SYM_INFO,
        &format!("v{}", info.version),
    );
    printer.print_status_line("Location", ANSI_BLUE, SYM_INFO, BINARY_PATH);
    printer.print_status_line("Size", ANSI_BLUE, SYM_INFO, &info.size_human);
    printer.print_status_line("Modified", ANSI_BLUE, SYM_INFO, &info.modified_local);
}

// =============================================================================
/// Renders the single-line `Dependencies:` summary row.
fn render_dependencies_status_line(
    report: &SetupStatusReport,
    skip_deps: bool,
    printer: ConsolePrinter,
) {
    if skip_deps {
        printer.print_status_line(
            "Dependencies",
            ANSI_YELLOW,
            SYM_ERR,
            "Check skipped (--no-deps)",
        );
    } else if report.dependencies.all_satisfied() {
        printer.print_status_line("Dependencies", ANSI_GREEN, SYM_OK, "All satisfied");
    } else {
        printer.print_status_line("Dependencies", ANSI_RED, SYM_ERR, "Missing packages");
    }
}

// =============================================================================
/// Renders the single-line `Service:` summary row when the binary is installed.
fn render_service_status_line(report: &SetupStatusReport, printer: ConsolePrinter) {
    if !report.binary_installed {
        return;
    }
    let summary = report.service.summary_label();
    if report.service.is_running_and_enabled() {
        printer.print_status_line("Service", ANSI_GREEN, SYM_OK, &summary);
    } else if summary == "not installed" {
        printer.print_status_line("Service", ANSI_RED, SYM_ERR, &summary);
    } else {
        printer.print_status_line("Service", ANSI_YELLOW, SYM_ERR, &summary);
    }
}

// =============================================================================
/// Renders `═══ System Requirements ═══`.
pub fn show_system_requirements(printer: ConsolePrinter) {
    let os_info = read_os_pretty_name();
    let deps_list: Vec<&str> = REQUIRED_DEPENDENCIES.iter().map(|d| d.binary).collect();
    printer.print_header("System Requirements");
    eprintln!("Required for {SOFTWARE_NAME}:");
    eprintln!("  • Operating System: {os_info}");
    eprintln!("  • Required binaries: {}", deps_list.join(", "));
    eprintln!("  • Installation path: {BINARY_PATH}");
    eprintln!("  • Service file:      {SYSTEMD_UNIT_PATH}");
}

// =============================================================================
/// Prints detailed multi-line version metadata to `stdout` (`setup version`).
pub fn show_detailed_version() {
    write_stdout_line(&format!(
        "{SOFTWARE_NAME}\nVersion: v{VERSION} ({LAST_MODIFIED})\nAuthor: {AUTHORS}\nLicense: {LICENSE}"
    ));
}

// =============================================================================
/// Displays detailed FHS, dependency, internet, and `systemd` status (`setup status`).
pub fn show_detailed_setup_status(skip_deps: bool, printer: ConsolePrinter) {
    let report = inspect_setup_status(true);
    printer.print_header(&format!("{SOFTWARE_NAME} Setup Status"));
    render_software_status_lines(&report, printer);
    render_dependencies_status_line(&report, skip_deps, printer);
    render_service_status_line(&report, printer);
    if report.internet_connected {
        printer.print_status_line("Internet", ANSI_GREEN, SYM_OK, "Connected");
    } else {
        printer.print_status_line("Internet", ANSI_YELLOW, SYM_ERR, "Not available");
    }

    eprintln!("\nFHS Paths:");
    eprintln!("  • Binary:          {BINARY_PATH}");
    eprintln!("  • Config file:     {}", report.config_path.display());
    eprintln!("  • State directory: {STATE_DIR}");
    eprintln!("  • Runtime path:    {RUN_DIR}");
    if ENABLE_DESKTOP_ENTRY {
        eprintln!("  • Desktop entry:   {DESKTOP_ENTRY_PATH}");
    }
    eprintln!("  • Service unit:    {SYSTEMD_UNIT_PATH}");
}
