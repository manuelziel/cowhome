//! Non-privileged installation status, binary metadata (`Size`, `Modified`), and probes.
//!
//! Separated from [`crate::setup::installer`] mutations so status inspection remains
//! read-only, compact, and usable without root privileges.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::core::constants::fhs_paths::{
    BINARY_PATH, CONFIG_DIR, CONFIG_FILE, DESKTOP_ENTRY_PATH, STATE_DIR,
};
use crate::core::process::run_command_captured;
use crate::core::security::{PathPresence, probe_path_presence};
use crate::core::time_util::format_epoch_local_human;
use crate::setup::deps::{DependencyReport, check_internet_connected, inspect_dependencies};
use crate::setup::systemd::{ServiceUnitStatus, query_unit_status};

// =============================================================================
/// Metadata of the installed `/usr/local/bin/<name>` binary (`Version`, `Size`, `Modified`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBinaryInfo {
    /// Semantic version string reported by `<binary> --version`.
    pub version: String,
    /// Human-readable file size (e.g., `"420K"`).
    pub size_human: String,
    /// Local modification timestamp (`"YYYY-MM-DD HH:MM:SS"`).
    pub modified_local: String,
}

// =============================================================================
/// Comprehensive snapshot of installation paths, dependencies, network, and `systemd` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStatusReport {
    /// Whether `/usr/local/bin/<name>` exists.
    pub binary_installed: bool,
    /// Metadata for `/usr/local/bin/<name>` when installed.
    pub installed_info: Option<InstalledBinaryInfo>,
    /// Target FHS binary path.
    pub installed_binary_path: PathBuf,
    /// Whether `/etc/<name>/config.toml` exists.
    pub config_installed: bool,
    /// Target FHS configuration path.
    pub config_path: PathBuf,
    /// Whether `/var/lib/<name>` exists.
    pub state_dir_exists: bool,
    /// Whether `/usr/share/applications/<name>.desktop` exists.
    pub desktop_entry_installed: bool,
    /// Outbound internet connectivity result (when probed).
    pub internet_connected: bool,
    /// System package manager and dependency probe results.
    pub dependencies: DependencyReport,
    /// `systemd` unit registration and active state.
    pub service: ServiceUnitStatus,
}

// =============================================================================
/// Returns `true` if `/usr/local/bin/<name>` is currently installed on the system.
#[must_use]
pub fn is_installed() -> bool {
    Path::new(BINARY_PATH).exists()
}

// =============================================================================
/// Queries the version string of `/usr/local/bin/<name>` via `--version`.
#[must_use]
pub fn get_installed_version() -> String {
    if !is_installed() {
        return String::from("not installed");
    }
    let Ok(out) = run_command_captured(BINARY_PATH, &["--version"], Duration::from_secs(3)) else {
        return String::from("unknown");
    };
    extract_semver_token(&out.stdout).unwrap_or_else(|| String::from("unknown"))
}

// =============================================================================
/// Collects installation, dependency, `systemd`, and optional internet status.
#[must_use]
pub fn inspect_setup_status(probe_internet: bool) -> SetupStatusReport {
    let bin_path = Path::new(BINARY_PATH);
    let binary_installed = is_installed();
    let installed_info = binary_installed.then(|| read_installed_binary_info(bin_path));

    let cfg_presence = probe_path_presence(Path::new(CONFIG_FILE));
    let config_installed = cfg_presence == PathPresence::Accessible
        || (cfg_presence == PathPresence::PermissionDenied && Path::new(CONFIG_DIR).exists());
    let state_presence = probe_path_presence(Path::new(STATE_DIR));

    SetupStatusReport {
        binary_installed,
        installed_info,
        installed_binary_path: bin_path.to_path_buf(),
        config_installed,
        config_path: PathBuf::from(CONFIG_FILE),
        state_dir_exists: state_presence != PathPresence::Missing,
        desktop_entry_installed: Path::new(DESKTOP_ENTRY_PATH).exists(),
        internet_connected: probe_internet && check_internet_connected(),
        dependencies: inspect_dependencies(),
        service: query_unit_status(),
    }
}

// =============================================================================
/// Extracts a semantic version token (e.g., `"0.0.1"`) from `--version` output.
fn extract_semver_token(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let trimmed = token.strip_prefix('v').unwrap_or(token);
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.') {
            return Some(trimmed.to_owned());
        }
    }
    None
}

// =============================================================================
/// Reads version, human-readable size, and formatted mtime of `bin_path`.
fn read_installed_binary_info(bin_path: &Path) -> InstalledBinaryInfo {
    let version = get_installed_version();
    let (size_human, modified_local) = match fs::metadata(bin_path) {
        Ok(meta) => (format_human_bytes(meta.len()), format_metadata_mtime(&meta)),
        Err(_) => (String::from("unknown"), String::from("unknown")),
    };
    InstalledBinaryInfo {
        version,
        size_human,
        modified_local,
    }
}

// =============================================================================
/// Formats a file metadata modification time as local `"YYYY-MM-DD HH:MM:SS"`.
fn format_metadata_mtime(meta: &fs::Metadata) -> String {
    let Ok(modified) = meta.modified() else {
        return String::from("unknown");
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => format_epoch_local_human(dur.as_secs()),
        Err(_) => String::from("unknown"),
    }
}

// =============================================================================
/// Formats byte counts compactly (`420K`, `1.2M`, `512B`).
#[must_use]
pub fn format_human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{}K", bytes.div_ceil(KIB))
    } else {
        format!("{bytes}B")
    }
}
