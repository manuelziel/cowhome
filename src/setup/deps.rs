//! Host OS detection, internet connectivity probes, and multi-distro package checks.
//!
//! Detects `apt-get`, `dnf`, `yum`, `pacman`, or `zypper` and installs any missing
//! system binaries declared in [`REQUIRED_DEPENDENCIES`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::constants::service_defaults::{
    NET_HTTP_TIMEOUT_SECS, NET_PING_TIMEOUT_SECS, NET_PROBE_IP, NET_PROBE_URL,
    PKG_INSTALL_TIMEOUT_SECS,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::process::{run_command_captured, run_command_inherited};
use crate::setup::SetupEvent;

/// Standard FHS executable search paths including `sbin` for unprivileged status checks.
const STANDARD_BIN_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

// =============================================================================
/// Declarative per-distribution package mapping for an external host binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemDependency {
    /// Executable binary name probed in `PATH` / `STANDARD_BIN_DIRS`.
    pub binary: &'static str,
    /// Debian / Ubuntu / Raspberry Pi OS package name.
    pub apt: &'static str,
    /// Fedora / RHEL / Rocky / AlmaLinux package name.
    pub dnf: &'static str,
    /// Arch Linux package name.
    pub pacman: &'static str,
    /// openSUSE package name.
    pub zypper: &'static str,
}

/// Required external system binaries for this project (extend per derived project).
pub const REQUIRED_DEPENDENCIES: &[SystemDependency] = &[SystemDependency {
    binary: "systemctl",
    apt: "systemd",
    dnf: "systemd",
    pacman: "systemd",
    zypper: "systemd",
}];

// =============================================================================
/// Supported Linux system package managers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// Debian, Ubuntu, Raspberry Pi OS (`apt-get`).
    Apt,
    /// Fedora, RHEL, Rocky, AlmaLinux (`dnf`).
    Dnf,
    /// Legacy RHEL / CentOS (`yum`).
    Yum,
    /// Arch Linux (`pacman`).
    Pacman,
    /// openSUSE (`zypper`).
    Zypper,
}

impl PackageManager {
    /// Executable binary name of the package manager.
    #[must_use]
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Yum => "yum",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
        }
    }

    /// Resolves the distribution-specific package name for `dep`.
    #[must_use]
    pub const fn package_for(self, dep: &SystemDependency) -> &'static str {
        match self {
            Self::Apt => dep.apt,
            Self::Dnf | Self::Yum => dep.dnf,
            Self::Pacman => dep.pacman,
            Self::Zypper => dep.zypper,
        }
    }
}

// =============================================================================
/// Snapshot of host runtime dependencies declared in [`REQUIRED_DEPENDENCIES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReport {
    /// Detected host package manager, if supported.
    pub package_manager: Option<PackageManager>,
    /// True if `/run/systemd/system` exists (`systemd` running as PID 1).
    pub systemd_booted: bool,
    /// Names of missing binaries from [`REQUIRED_DEPENDENCIES`].
    pub missing_binaries: Vec<&'static str>,
}

impl DependencyReport {
    /// Returns `true` when every binary in [`REQUIRED_DEPENDENCIES`] is present.
    #[must_use]
    pub fn all_satisfied(&self) -> bool {
        self.missing_binaries.is_empty()
    }
}

// =============================================================================
/// Inspects the host for all binaries in [`REQUIRED_DEPENDENCIES`].
#[must_use]
pub fn inspect_dependencies() -> DependencyReport {
    let missing_binaries = REQUIRED_DEPENDENCIES
        .iter()
        .filter(|dep| find_system_binary(dep.binary).is_none())
        .map(|dep| dep.binary)
        .collect();

    DependencyReport {
        package_manager: detect_package_manager(),
        systemd_booted: Path::new("/run/systemd/system").exists(),
        missing_binaries,
    }
}

// =============================================================================
/// Probes internet connectivity via ICMP `ping`, falling back to `curl`/`wget` HTTPS HEAD.
#[must_use]
pub fn check_internet_connected() -> bool {
    let timeout_str = NET_PING_TIMEOUT_SECS.to_string();
    let wall_timeout = Duration::from_secs(NET_PING_TIMEOUT_SECS + 2);
    if run_command_captured(
        "ping",
        &["-c", "1", "-W", &timeout_str, "--", NET_PROBE_IP],
        wall_timeout,
    )
    .is_ok_and(|out| out.success)
    {
        return true;
    }
    probe_http_connectivity()
}

// =============================================================================
/// Fallback HTTP 204 connectivity probe (`curl` or `wget`) when ICMP is blocked.
fn probe_http_connectivity() -> bool {
    let http_sec = NET_HTTP_TIMEOUT_SECS.to_string();
    let wall_timeout = Duration::from_secs(NET_HTTP_TIMEOUT_SECS + 2);
    if find_system_binary("curl").is_some()
        && run_command_captured(
            "curl",
            &[
                "-fsS",
                "--connect-timeout",
                &http_sec,
                "--head",
                "--",
                NET_PROBE_URL,
            ],
            wall_timeout,
        )
        .is_ok_and(|out| out.success)
    {
        return true;
    }
    let wget_timeout = format!("--timeout={http_sec}");
    find_system_binary("wget").is_some()
        && run_command_captured(
            "wget",
            &["-q", "--spider", &wget_timeout, "--", NET_PROBE_URL],
            wall_timeout,
        )
        .is_ok_and(|out| out.success)
}

// =============================================================================
/// Reads `PRETTY_NAME` (or `NAME`) from `/etc/os-release` for `setup requirements`.
#[must_use]
pub fn read_os_pretty_name() -> String {
    let Ok(raw) = fs::read_to_string("/etc/os-release") else {
        return String::from("Linux");
    };
    for key in ["PRETTY_NAME=", "NAME="] {
        for line in raw.lines() {
            if let Some(val) = line.trim().strip_prefix(key) {
                let cleaned = val.trim().trim_matches('"').trim_matches('\'');
                if !cleaned.is_empty() {
                    return cleaned.to_owned();
                }
            }
        }
    }
    String::from("Linux")
}

// =============================================================================
/// Detects the first available system package manager on the host.
#[must_use]
pub fn detect_package_manager() -> Option<PackageManager> {
    [
        PackageManager::Apt,
        PackageManager::Dnf,
        PackageManager::Yum,
        PackageManager::Pacman,
        PackageManager::Zypper,
    ]
    .into_iter()
    .find(|pm| find_system_binary(pm.binary_name()).is_some())
}

// =============================================================================
/// Searches `PATH` and standard FHS `bin`/`sbin` directories for `binary_name`.
#[must_use]
pub fn find_system_binary(binary_name: &str) -> Option<PathBuf> {
    if let Some(path_os) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_os) {
            let candidate = dir.join(binary_name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    for std_dir in STANDARD_BIN_DIRS {
        let candidate = Path::new(std_dir).join(binary_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

// =============================================================================
/// Checks whether `path` is an existing regular file with execute permission bits.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

// =============================================================================
/// Verifies required system binaries and installs missing packages automatically.
///
/// # Errors
/// Returns [`AppError::Setup`] if packages cannot be installed or remain missing.
pub fn ensure_dependencies_installed(
    skip_deps: bool,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if skip_deps {
        on_event(SetupEvent::Skipped(
            "Skipping dependency checks (--no-deps)".into(),
        ));
        return Ok(());
    }
    let report = inspect_dependencies();
    if report.all_satisfied() {
        on_event(SetupEvent::Completed("All dependencies satisfied".into()));
        return Ok(());
    }
    let pm = report.package_manager.ok_or_else(|| {
        AppError::Setup(format!(
            "Missing dependencies ({}) and no supported package manager found",
            report.missing_binaries.join(", ")
        ))
    })?;

    for dep in REQUIRED_DEPENDENCIES {
        if find_system_binary(dep.binary).is_none() {
            install_single_package(pm, pm.package_for(dep), logger, on_event)?;
        }
    }
    on_event(SetupEvent::Completed(
        "Dependencies installed successfully".into(),
    ));
    Ok(())
}

// =============================================================================
/// Invokes `pm` non-interactively to install `pkg_name`.
///
/// # Errors
/// Returns [`AppError`] if the package manager fails or times out.
fn install_single_package(
    pm: PackageManager,
    pkg_name: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    logger.info(&format!(
        "Installing dependency '{pkg_name}' via {}",
        pm.binary_name()
    ));
    on_event(SetupEvent::Step(format!(
        "Installing package via {}: {pkg_name}",
        pm.binary_name()
    )));
    if !run_pm_install_cmd(pm, pkg_name)? {
        return Err(AppError::Setup(format!(
            "Package installation failed for '{pkg_name}'"
        )));
    }
    Ok(())
}

// =============================================================================
/// Dispatches the distribution-specific package manager command for `pkg_name`.
///
/// # Errors
/// Returns [`AppError`] if spawning or waiting for the package manager fails.
fn run_pm_install_cmd(pm: PackageManager, pkg_name: &str) -> AppResult<bool> {
    let timeout = Duration::from_secs(PKG_INSTALL_TIMEOUT_SECS);
    match pm {
        PackageManager::Apt => Ok(run_command_inherited("apt-get", &["update"], timeout)?
            && run_command_inherited("apt-get", &["install", "-y", pkg_name], timeout)?),
        PackageManager::Dnf => run_command_inherited("dnf", &["install", "-y", pkg_name], timeout),
        PackageManager::Yum => run_command_inherited("yum", &["install", "-y", pkg_name], timeout),
        PackageManager::Pacman => {
            run_command_inherited("pacman", &["-Syu", "--noconfirm", pkg_name], timeout)
        }
        PackageManager::Zypper => run_command_inherited(
            "zypper",
            &["--non-interactive", "install", pkg_name],
            timeout,
        ),
    }
}
