//! FHS 3.0 installation, zero-data-loss update, reinstall, uninstall, and purge workflows.
//!
//! Copies `std::env::current_exe()` atomically to `/usr/local/bin/cowhome`, provisions
//! `/etc/cowhome/config.toml`, `/var/lib/cowhome`, the `cowhome` system account, and
//! `cowhome.service` without external runtime shell libraries.

use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::core::constants::app_meta::{SOFTWARE_NAME, VERSION};
use crate::core::constants::embedded_assets::DEFAULT_CONFIG_TOML;
use crate::core::constants::fhs_paths::{
    BINARY_PATH, CONFIG_DIR, CONFIG_FILE, DESKTOP_ENTRY_PATH, RUN_DIR, STATE_DIR,
    XDG_AUTOSTART_PATH,
};
use crate::core::constants::service_defaults::{
    BINARY_FILE_MODE, GROUP_CONFIG_MODE, SERVICE_GROUP,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::security::{
    apply_ownership_if_root, atomic_write_file, ensure_not_symlink, ensure_root_privileges,
};
use crate::setup::deps::ensure_dependencies_installed;
pub use crate::setup::status::{
    get_installed_version, inspect_setup_status, is_installed, SetupStatusReport,
};
use crate::setup::systemd::{
    install_or_update_unit, restart_unit_after_update, stop_disable_and_remove_unit,
    stop_unit_for_update,
};
use crate::setup::users::{
    ensure_service_account, provision_fhs_directories, remove_service_account_on_purge,
};
use crate::setup::SetupEvent;

// =============================================================================
/// Performs a full system installation (`cowhome setup install`).
///
/// # Errors
/// Returns [`AppError`] if already installed without `force`, or if writes fail.
pub fn install(
    force: bool,
    skip_deps: bool,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_root_privileges()?;
    if is_installed() && !force {
        on_event(SetupEvent::Warning("Software is already installed".into()));
        on_event(SetupEvent::Step(
            "Use --force or reinstall to reinstall".into(),
        ));
        return Err(AppError::Setup("Software is already installed".into()));
    }
    provision_all_system_files(skip_deps, logger, on_event)?;
    logger.info(&format!("Completed {SOFTWARE_NAME} system installation"));
    Ok(())
}

// =============================================================================
/// Updates binary and unit while preserving `/etc/cowhome/config.toml` & `/var/lib/cowhome`.
///
/// # Errors
/// Returns [`AppError`] if not installed, privileges are missing, or FHS updates fail.
pub fn update(
    skip_deps: bool,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_root_privileges()?;
    if !is_installed() {
        on_event(SetupEvent::Step("Run './setup.sh install' first".into()));
        return Err(AppError::Setup(format!("{SOFTWARE_NAME} is not installed")));
    }
    let current_ver = get_installed_version();
    on_event(SetupEvent::Step(format!(
        "Current version: {current_ver} -> New version: {VERSION}"
    )));

    let was_running = stop_unit_for_update(logger, on_event);
    provision_all_system_files(skip_deps, logger, on_event)?;
    if was_running {
        restart_unit_after_update(logger, on_event);
    }
    logger.info(&format!("Completed {SOFTWARE_NAME} system update"));
    Ok(())
}

// =============================================================================
/// Reinstalls all system files while preserving an existing `/etc/cowhome/config.toml`.
///
/// # Errors
/// Returns [`AppError`] if root privileges, dependencies, or FHS writes fail.
pub fn reinstall(
    skip_deps: bool,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_root_privileges()?;
    let was_running = stop_unit_for_update(logger, on_event);
    provision_all_system_files(skip_deps, logger, on_event)?;
    if was_running {
        restart_unit_after_update(logger, on_event);
    }
    logger.info(&format!("Completed {SOFTWARE_NAME} system reinstallation"));
    Ok(())
}

// =============================================================================
/// Shared provisioning pipeline used by `install`, `update`, and `reinstall`.
///
/// # Errors
/// Returns [`AppError`] if dependency installation, user setup, or file deployment fails.
fn provision_all_system_files(
    skip_deps: bool,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_dependencies_installed(skip_deps, logger, on_event)?;
    ensure_service_account(logger, on_event)?;
    provision_fhs_directories(on_event)?;
    install_self_binary(logger, on_event)?;
    install_default_config_if_missing(logger, on_event)?;
    install_or_update_unit(logger, on_event)
}

// =============================================================================
/// Removes binary and `.service` while preserving `/etc/cowhome` & `/var/lib/cowhome`.
///
/// # Errors
/// Returns [`AppError`] if root privileges are missing or teardown fails.
pub fn uninstall(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) -> AppResult<()> {
    ensure_root_privileges()?;
    cleanup_installed_artifacts(logger, on_event)?;
    logger.info(&format!("Completed {SOFTWARE_NAME} uninstallation"));
    Ok(())
}

// =============================================================================
/// Performs `uninstall` and deletes `/var/lib/cowhome`, `/etc/cowhome`, and `cowhome` user.
///
/// # Errors
/// Returns [`AppError`] if root privileges are missing or directory deletion fails.
pub fn purge(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) -> AppResult<()> {
    ensure_root_privileges()?;
    cleanup_installed_artifacts(logger, on_event)?;
    remove_dir_if_present(Path::new(STATE_DIR), "Data directory", logger, on_event)?;
    remove_dir_if_present(Path::new(CONFIG_DIR), "Config directory", logger, on_event)?;
    remove_service_account_on_purge(logger, on_event);
    logger.info(&format!("Completed {SOFTWARE_NAME} purge"));
    Ok(())
}

// =============================================================================
/// Removes binary, unit, optional desktop files, and volatile `/run/cowhome` directory.
///
/// # Errors
/// Returns [`AppError`] if stopping the unit or removing installed files fails.
fn cleanup_installed_artifacts(
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    stop_disable_and_remove_unit(logger, on_event)?;
    remove_binary_or_symlink(Path::new(BINARY_PATH), logger, on_event)?;
    remove_dir_if_present(Path::new(RUN_DIR), "Runtime directory", logger, on_event)?;
    remove_file_if_present(
        Path::new(DESKTOP_ENTRY_PATH),
        "Desktop entry",
        logger,
        on_event,
    )?;
    remove_file_if_present(
        Path::new(XDG_AUTOSTART_PATH),
        "Autostart entry",
        logger,
        on_event,
    )
}

// =============================================================================
/// Copies `std::env::current_exe()` atomically to `/usr/local/bin/cowhome` (`0755`).
///
/// # Errors
/// Returns [`AppError`] if resolving `current_exe()` or writing `/usr/local/bin/cowhome` fails.
fn install_self_binary(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) -> AppResult<()> {
    let current_exe = std::env::current_exe()
        .map_err(|err| AppError::io("Failed resolving current executable path", err))?;
    let dest = Path::new(BINARY_PATH);
    unlink_if_symlink(dest, logger, on_event)?;

    if same_inode(&current_exe, dest) {
        fs::set_permissions(dest, Permissions::from_mode(BINARY_FILE_MODE))
            .map_err(|err| AppError::io(format!("Failed chmod on '{BINARY_PATH}'"), err))?;
        on_event(SetupEvent::Completed(format!(
            "Binary verified: {BINARY_PATH}"
        )));
        return Ok(());
    }

    let bytes = fs::read(&current_exe).map_err(|err| {
        AppError::io(
            format!("Failed reading executable '{}'", current_exe.display()),
            err,
        )
    })?;
    atomic_write_file(dest, &bytes, BINARY_FILE_MODE)?;
    logger.info(&format!("Installed binary to '{BINARY_PATH}'"));
    on_event(SetupEvent::Completed(format!(
        "Binary installed: {BINARY_PATH}"
    )));
    Ok(())
}

// =============================================================================
/// Unlinks `/usr/local/bin/cowhome` if it is currently a symbolic link before writing ELF.
///
/// # Errors
/// Returns [`AppError`] if the parent directory is a symlink or unlinking fails.
fn unlink_if_symlink(
    dest: &Path,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if let Some(parent) = dest.parent() {
        ensure_not_symlink(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(dest).map_err(|err| {
            AppError::io(format!("Failed removing symlink '{}'", dest.display()), err)
        })?;
        logger.info(&format!("Removed symlink '{}'", dest.display()));
        on_event(SetupEvent::Completed(format!(
            "Symlink removed: {}",
            dest.display()
        )));
    }
    Ok(())
}

// =============================================================================
/// Removes `/usr/local/bin/cowhome` whether it is a regular binary or symlink.
///
/// # Errors
/// Returns [`AppError`] if the parent directory is a symlink or removal fails.
fn remove_binary_or_symlink(
    dest: &Path,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if let Some(parent) = dest.parent() {
        ensure_not_symlink(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest)
            .map_err(|err| AppError::io(format!("Failed removing '{}'", dest.display()), err))?;
        logger.info(&format!("Removed installed binary '{}'", dest.display()));
        on_event(SetupEvent::Completed(format!(
            "Binary removed: {}",
            dest.display()
        )));
    }
    Ok(())
}

// =============================================================================
/// Checks whether two paths refer to the exact same POSIX device and inode.
fn same_inode(path_a: &Path, path_b: &Path) -> bool {
    match (fs::metadata(path_a), fs::metadata(path_b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

// =============================================================================
/// Writes `/etc/cowhome/config.toml` (`0640`, `root:cowhome`) only when absent.
///
/// # Errors
/// Returns [`AppError`] if `CONFIG_FILE` is a symlink or atomic creation fails.
fn install_default_config_if_missing(
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    let cfg_path = Path::new(CONFIG_FILE);
    ensure_not_symlink(cfg_path)?;
    if cfg_path.exists() {
        on_event(SetupEvent::Skipped(format!(
            "Configuration preserved: {CONFIG_FILE}"
        )));
        return Ok(());
    }
    atomic_write_file(cfg_path, DEFAULT_CONFIG_TOML.as_bytes(), GROUP_CONFIG_MODE)?;
    apply_ownership_if_root(cfg_path, "root", SERVICE_GROUP)?;
    logger.info(&format!(
        "Provisioned default configuration '{CONFIG_FILE}'"
    ));
    on_event(SetupEvent::Completed(format!(
        "Config installed: {CONFIG_FILE}"
    )));
    Ok(())
}

// =============================================================================
/// Deletes a regular file if present after ensuring it is not a symlink.
///
/// # Errors
/// Returns [`AppError`] if `path` is a symlink or file removal fails.
fn remove_file_if_present(
    path: &Path,
    label: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_not_symlink(path)?;
    if path.exists() {
        fs::remove_file(path)
            .map_err(|err| AppError::io(format!("Failed removing '{}'", path.display()), err))?;
        logger.info(&format!("Removed {label} '{}'", path.display()));
        on_event(SetupEvent::Completed(format!(
            "{label} removed: {}",
            path.display()
        )));
    }
    Ok(())
}

// =============================================================================
/// Deletes a directory tree if present after ensuring the root path is not a symlink.
///
/// # Errors
/// Returns [`AppError`] if `dir` is a symlink or recursive removal fails.
fn remove_dir_if_present(
    dir: &Path,
    label: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_not_symlink(dir)?;
    if dir.exists() {
        fs::remove_dir_all(dir)
            .map_err(|err| AppError::io(format!("Failed removing '{}'", dir.display()), err))?;
        logger.info(&format!("Removed {label} '{}'", dir.display()));
        on_event(SetupEvent::Completed(format!(
            "{label} removed: {}",
            dir.display()
        )));
    }
    Ok(())
}
