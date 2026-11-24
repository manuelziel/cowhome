//! `systemd` unit template rendering, registration, runtime control, and teardown.
//!
//! Renders `assets/template.service` with compile-time FHS constants, strips unused
//! empty directives cleanly, reloads `systemd`, and supports `start` / `stop` / `restart`.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::core::constants::app_meta::{NAME, SOFTWARE_NAME};
use crate::core::constants::embedded_assets::SYSTEMD_UNIT_TEMPLATE;
use crate::core::constants::fhs_paths::{
    BINARY_PATH, RUN_DIR, STATE_DIR, SYSTEMD_UNIT_NAME, SYSTEMD_UNIT_PATH,
};
use crate::core::constants::service_defaults::{
    CREATE_SERVICE, PUBLIC_FILE_MODE, SERVICE_ENVIRONMENT, SERVICE_EXEC_STOP,
    SERVICE_EXEC_SUBCOMMAND, SERVICE_GROUP, SERVICE_NO_NEW_PRIVILEGES, SERVICE_PART_OF,
    SERVICE_PRIVATE_DEVICES, SERVICE_PRIVATE_TMP, SERVICE_PROTECT_HOME, SERVICE_PROTECT_SYSTEM,
    SERVICE_READ_WRITE_PATHS, SERVICE_RESTART, SERVICE_RESTART_SEC, SERVICE_SUPPLEMENTARY_GROUPS,
    SERVICE_TIMEOUT_START_SEC, SERVICE_TIMEOUT_STOP_SEC, SERVICE_TYPE, SERVICE_UMASK, SERVICE_USER,
    SUBPROCESS_TIMEOUT_SECS, SUDO_TIMEOUT_SECS,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::process::{run_command_captured, run_command_inherited};
use crate::core::security::{atomic_write_file, ensure_not_symlink, is_root};
use crate::setup::SetupEvent;
use crate::setup::deps::find_system_binary;

// =============================================================================
/// Live registration and activation state of `<name>.service`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnitStatus {
    /// Whether `/etc/systemd/system/<name>.service` exists on disk.
    pub unit_file_installed: bool,
    /// Whether `systemd` is active as PID 1 on this host.
    pub systemd_available: bool,
    /// Output of `systemctl is-enabled` (e.g., `"enabled"`, `"disabled"`).
    pub enabled_state: String,
    /// Output of `systemctl is-active` (e.g., `"active"`, `"inactive"`).
    pub active_state: String,
}

impl ServiceUnitStatus {
    /// Formats `"running (enabled)"`, `"stopped (disabled)"`, or `"not installed"`.
    #[must_use]
    pub fn summary_label(&self) -> String {
        if !self.unit_file_installed {
            return String::from("not installed");
        }
        match (
            self.active_state == "active",
            self.enabled_state == "enabled",
        ) {
            (true, true) => String::from("running (enabled)"),
            (true, false) => String::from("running (disabled)"),
            (false, true) => String::from("stopped (enabled)"),
            (false, false) => String::from("stopped (disabled)"),
        }
    }

    /// Returns `true` when the unit is installed, active, and enabled.
    #[must_use]
    pub fn is_running_and_enabled(&self) -> bool {
        self.unit_file_installed && self.active_state == "active" && self.enabled_state == "enabled"
    }
}

// =============================================================================
/// Renders `assets/template.service` and strips lines whose placeholder resolved empty.
#[must_use]
pub fn render_systemd_unit() -> String {
    let exec_start = if SERVICE_EXEC_SUBCOMMAND.is_empty() {
        BINARY_PATH.to_owned()
    } else {
        format!("{BINARY_PATH} {SERVICE_EXEC_SUBCOMMAND}")
    };
    let restart_sec = SERVICE_RESTART_SEC.to_string();
    let start_sec = SERVICE_TIMEOUT_START_SEC.to_string();
    let stop_sec = SERVICE_TIMEOUT_STOP_SEC.to_string();
    let mut rendered = SYSTEMD_UNIT_TEMPLATE.to_owned();
    for (token, value) in unit_placeholder_pairs(&exec_start, &restart_sec, &start_sec, &stop_sec) {
        rendered = rendered.replace(token, value);
    }
    strip_empty_unit_directives(&rendered)
}

// =============================================================================
/// Returns all `(placeholder, value)` pairs applied to `assets/template.service`.
fn unit_placeholder_pairs<'a>(
    exec_start: &'a str,
    restart_sec: &'a str,
    start_sec: &'a str,
    stop_sec: &'a str,
) -> [(&'static str, &'a str); 21] {
    [
        ("{{SOFTWARE_NAME}}", SOFTWARE_NAME),
        ("{{SOFTWARE_DIR_NAME}}", NAME),
        ("{{SERVICE_PART_OF}}", SERVICE_PART_OF),
        ("{{SERVICE_TYPE}}", SERVICE_TYPE),
        ("{{SERVICE_USER}}", SERVICE_USER),
        ("{{SERVICE_GROUP}}", SERVICE_GROUP),
        ("{{SERVICE_UMASK}}", SERVICE_UMASK),
        (
            "{{SERVICE_SUPPLEMENTARY_GROUPS}}",
            SERVICE_SUPPLEMENTARY_GROUPS,
        ),
        ("{{SERVICE_ENVIRONMENT}}", SERVICE_ENVIRONMENT),
        ("{{SERVICE_EXEC_START}}", exec_start),
        ("{{SERVICE_EXEC_STOP}}", SERVICE_EXEC_STOP),
        ("{{SERVICE_RESTART}}", SERVICE_RESTART),
        ("{{SERVICE_RESTART_SEC}}", restart_sec),
        ("{{SERVICE_TIMEOUT_START_SEC}}", start_sec),
        ("{{SERVICE_TIMEOUT_STOP_SEC}}", stop_sec),
        ("{{SERVICE_NO_NEW_PRIVILEGES}}", SERVICE_NO_NEW_PRIVILEGES),
        ("{{SERVICE_PRIVATE_TMP}}", SERVICE_PRIVATE_TMP),
        ("{{SERVICE_PRIVATE_DEVICES}}", SERVICE_PRIVATE_DEVICES),
        ("{{SERVICE_PROTECT_SYSTEM}}", SERVICE_PROTECT_SYSTEM),
        ("{{SERVICE_PROTECT_HOME}}", SERVICE_PROTECT_HOME),
        ("{{SERVICE_READ_WRITE_PATHS}}", SERVICE_READ_WRITE_PATHS),
    ]
}

// =============================================================================
/// Drops optional `Key=` lines whose value evaluated to empty so `systemd` emits zero warnings.
fn strip_empty_unit_directives(unit_body: &str) -> String {
    let with_dirs = unit_body
        .replace("{{STATE_DIR}}", STATE_DIR)
        .replace("{{RUN_DIR}}", RUN_DIR);
    let mut out = String::with_capacity(with_dirs.len());
    for line in with_dirs.lines() {
        let is_empty_kv = line
            .trim()
            .split_once('=')
            .is_some_and(|(_, val)| val.trim().trim_matches('"').is_empty());
        if !is_empty_kv {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// =============================================================================
/// Writes `/etc/systemd/system/<name>.service` atomically (`0644`) and runs `daemon-reload`.
///
/// # Errors
/// Returns [`AppError`] on symlink/write or `systemctl daemon-reload` failure.
pub fn install_or_update_unit(
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if !CREATE_SERVICE {
        return Ok(());
    }
    let rendered = render_systemd_unit();
    atomic_write_file(
        Path::new(SYSTEMD_UNIT_PATH),
        rendered.as_bytes(),
        PUBLIC_FILE_MODE,
    )?;
    logger.info(&format!("Installed systemd unit '{SYSTEMD_UNIT_PATH}'"));
    on_event(SetupEvent::Completed(format!(
        "Service unit installed: {SYSTEMD_UNIT_PATH}"
    )));
    reload_systemd_daemon(logger, on_event)
}

// =============================================================================
/// Executes `systemctl <start|stop|restart|reload> <name>.service` (elevating via `sudo` if needed).
///
/// # Errors
/// Returns [`AppError`] if the unit is not installed or `systemctl` fails.
pub fn control_service_unit(verb: &str, logger: &Logger) -> AppResult<()> {
    if !Path::new(SYSTEMD_UNIT_PATH).exists() {
        return Err(AppError::Service(format!(
            "Service unit '{SYSTEMD_UNIT_PATH}' is not installed (run 'sudo ./setup.sh install')"
        )));
    }
    if is_root() {
        let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
        let out = run_command_captured("systemctl", &[verb, SYSTEMD_UNIT_NAME], timeout)?;
        if !out.success {
            return Err(AppError::Service(format!(
                "'systemctl {verb} {SYSTEMD_UNIT_NAME}' failed: {}",
                out.stderr.trim()
            )));
        }
    } else {
        let timeout = Duration::from_secs(SUDO_TIMEOUT_SECS);
        if !run_command_inherited(
            "sudo",
            &["--", "systemctl", verb, SYSTEMD_UNIT_NAME],
            timeout,
        )? {
            return Err(AppError::Service(format!(
                "'sudo systemctl {verb} {SYSTEMD_UNIT_NAME}' failed"
            )));
        }
    }
    logger.info(&format!("Executed 'systemctl {verb} {SYSTEMD_UNIT_NAME}'"));
    Ok(())
}

// =============================================================================
/// Stops `<name>.service` before an update when currently active, returning `true` if stopped.
pub fn stop_unit_for_update(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) -> bool {
    let status = query_unit_status();
    if !status.systemd_available || status.active_state != "active" {
        return false;
    }
    on_event(SetupEvent::Step("Stopping service for update...".into()));
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    match run_command_captured("systemctl", &["stop", SYSTEMD_UNIT_NAME], timeout) {
        Ok(out) if out.success => {
            on_event(SetupEvent::Completed("Service stopped".into()));
            true
        }
        _ => {
            logger.warning("Failed to stop service before update");
            on_event(SetupEvent::Warning(
                "Failed to stop service, continuing anyway".into(),
            ));
            true
        }
    }
}

// =============================================================================
/// Restarts `<name>.service` after a successful update if it was previously running.
pub fn restart_unit_after_update(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) {
    on_event(SetupEvent::Step("Restarting service...".into()));
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    match run_command_captured("systemctl", &["restart", SYSTEMD_UNIT_NAME], timeout) {
        Ok(out) if out.success => {
            on_event(SetupEvent::Completed(
                "Service restarted successfully".into(),
            ));
        }
        _ => {
            logger.warning("Failed to restart service after update");
            on_event(SetupEvent::Warning(format!(
                "Failed to restart service - try: sudo systemctl restart {SYSTEMD_UNIT_NAME}"
            )));
        }
    }
}

// =============================================================================
/// Stops, disables, and deletes `<name>.service`, followed by `systemctl daemon-reload`.
///
/// # Errors
/// Returns [`AppError`] if symlink checks, file deletion, or `daemon-reload` fail.
pub fn stop_disable_and_remove_unit(
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    let unit_path = Path::new(SYSTEMD_UNIT_PATH);
    ensure_not_symlink(unit_path)?;
    if is_systemd_running() && unit_path.exists() {
        on_event(SetupEvent::Step(format!(
            "Stopping and disabling {SYSTEMD_UNIT_NAME}..."
        )));
        let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
        for verb in ["stop", "disable"] {
            if let Err(err) = run_command_captured("systemctl", &[verb, SYSTEMD_UNIT_NAME], timeout)
            {
                logger.debug(&format!("Ignoring 'systemctl {verb}' teardown note: {err}"));
            }
        }
    }
    if unit_path.exists() {
        fs::remove_file(unit_path)
            .map_err(|err| AppError::io(format!("Failed removing '{SYSTEMD_UNIT_PATH}'"), err))?;
        logger.info(&format!("Removed systemd unit '{SYSTEMD_UNIT_PATH}'"));
        on_event(SetupEvent::Completed(format!(
            "Systemd service removed: {SYSTEMD_UNIT_PATH}"
        )));
        reload_systemd_daemon(logger, on_event)?;
    }
    Ok(())
}

// =============================================================================
/// Queries non-privileged `systemd` unit installation, enablement, and active state.
#[must_use]
pub fn query_unit_status() -> ServiceUnitStatus {
    let unit_file_installed = Path::new(SYSTEMD_UNIT_PATH).exists();
    if !is_systemd_running() {
        return ServiceUnitStatus {
            unit_file_installed,
            systemd_available: false,
            enabled_state: String::from("unavailable"),
            active_state: String::from("unavailable"),
        };
    }
    ServiceUnitStatus {
        unit_file_installed,
        systemd_available: true,
        enabled_state: query_systemctl_line(&["is-enabled", SYSTEMD_UNIT_NAME]),
        active_state: query_systemctl_line(&["is-active", SYSTEMD_UNIT_NAME]),
    }
}

// =============================================================================
/// Triggers `systemctl daemon-reload` when `systemd` PID 1 is active on the host.
///
/// # Errors
/// Returns [`AppError`] if `systemctl daemon-reload` fails or times out.
fn reload_systemd_daemon(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) -> AppResult<()> {
    if !is_systemd_running() {
        on_event(SetupEvent::Skipped(
            "systemd PID 1 not active in this environment (skipped daemon-reload)".into(),
        ));
        return Ok(());
    }
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    let out = run_command_captured("systemctl", &["daemon-reload"], timeout)?;
    if !out.success {
        return Err(AppError::Setup(format!(
            "'systemctl daemon-reload' failed: {}",
            out.stderr.trim()
        )));
    }
    logger.debug("Executed 'systemctl daemon-reload'");
    Ok(())
}

// =============================================================================
/// Returns `true` if `/run/systemd/system` exists and `systemctl` is executable.
fn is_systemd_running() -> bool {
    Path::new("/run/systemd/system").exists() && find_system_binary("systemctl").is_some()
}

// =============================================================================
/// Reads single-line state text (`enabled`, `disabled`, `active`, `inactive`) from `systemctl`.
fn query_systemctl_line(args: &[&str]) -> String {
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    match run_command_captured("systemctl", args, timeout) {
        Ok(out) => {
            let line = out.stdout.lines().next().unwrap_or("").trim();
            if line.is_empty() {
                String::from("not-found")
            } else {
                line.to_owned()
            }
        }
        Err(_) => String::from("unknown"),
    }
}
