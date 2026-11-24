//! Dedicated system service account (`useradd --system`) and FHS directory ownership.
//!
//! Mirrors `02_linux/_project_template/lib/setup/users.sh`: optionally provisions an
//! unprivileged system user/group during `install` and assigns strict permissions
//! (`0750`) and ownership on `/etc/<name>` and `/var/lib/<name>`.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use crate::core::constants::fhs_paths::{CONFIG_DIR, STATE_DIR};
use crate::core::constants::service_defaults::{
    DIR_MODE, PRIVATE_DIR_MODE, SERVICE_CREATE_USER, SERVICE_GROUP, SERVICE_SUPPLEMENTARY_GROUPS,
    SERVICE_USER, SERVICE_USER_SHELL, SUBPROCESS_TIMEOUT_SECS,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::process::run_command_captured;
use crate::core::security::{
    apply_ownership_if_root, ensure_not_symlink, lookup_group_gid, lookup_passwd_by_name,
};
use crate::setup::SetupEvent;

// =============================================================================
/// Creates `SERVICE_GROUP` and `SERVICE_USER` when `SERVICE_CREATE_USER` is enabled.
///
/// # Errors
/// Returns [`AppError::Setup`] if `groupadd` or `useradd` fails.
pub fn ensure_service_account(
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if !SERVICE_CREATE_USER || SERVICE_USER == "root" {
        return Ok(());
    }
    ensure_system_group(SERVICE_GROUP, logger, on_event)?;
    ensure_system_user(SERVICE_USER, SERVICE_GROUP, logger, on_event)?;
    add_supplementary_groups(SERVICE_USER, SERVICE_SUPPLEMENTARY_GROUPS, logger, on_event);
    Ok(())
}

// =============================================================================
/// Removes `SERVICE_USER` during `purge` if `SERVICE_CREATE_USER` is enabled.
pub fn remove_service_account_on_purge(logger: &Logger, on_event: &mut dyn FnMut(SetupEvent)) {
    if !SERVICE_CREATE_USER || SERVICE_USER == "root" {
        return;
    }
    if lookup_passwd_by_name(SERVICE_USER).is_err() {
        return;
    }
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    if let Ok(out) = run_command_captured("userdel", &[SERVICE_USER], timeout)
        && out.success
    {
        logger.info(&format!("Removed system user '{SERVICE_USER}'"));
        on_event(SetupEvent::Completed(format!(
            "Removed system user: {SERVICE_USER}"
        )));
    }
}

// =============================================================================
/// Provisions `/etc/<name>` and `/var/lib/<name>` with hardened modes and ownership.
///
/// # Errors
/// Returns [`AppError`] on symlink detection, directory creation, or permission failure.
pub fn provision_fhs_directories(on_event: &mut dyn FnMut(SetupEvent)) -> AppResult<()> {
    let dir_mode = if SERVICE_USER == "root" && SERVICE_GROUP == "root" {
        DIR_MODE
    } else {
        PRIVATE_DIR_MODE
    };
    create_secure_dir(
        Path::new(CONFIG_DIR),
        dir_mode,
        "root",
        SERVICE_GROUP,
        on_event,
    )?;
    create_secure_dir(
        Path::new(STATE_DIR),
        dir_mode,
        SERVICE_USER,
        SERVICE_GROUP,
        on_event,
    )
}

// =============================================================================
/// Creates a single FHS directory (`CWE-59`-checked) and applies mode + ownership.
///
/// # Errors
/// Returns [`AppError`] if `dir` is a symlink or directory creation/`chmod`/`chown` fails.
fn create_secure_dir(
    dir: &Path,
    mode: u32,
    owner: &str,
    group: &str,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    ensure_not_symlink(dir)?;
    if !dir.exists() {
        fs::create_dir_all(dir)
            .map_err(|err| AppError::io(format!("Failed creating '{}'", dir.display()), err))?;
        on_event(SetupEvent::Completed(format!(
            "Directory created: {}",
            dir.display()
        )));
    }
    fs::set_permissions(dir, Permissions::from_mode(mode))
        .map_err(|err| AppError::io(format!("Failed chmod on '{}'", dir.display()), err))?;
    apply_ownership_if_root(dir, owner, group)
}

// =============================================================================
/// Ensures `group_name` exists in `/etc/group`, creating it with `groupadd --system`.
///
/// # Errors
/// Returns [`AppError`] if `groupadd` fails or times out.
fn ensure_system_group(
    group_name: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if group_name == "root" || lookup_group_gid(group_name).is_ok() {
        return Ok(());
    }
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    let out = run_command_captured("groupadd", &["--system", group_name], timeout)?;
    if !out.success {
        return Err(AppError::Setup(format!(
            "Failed creating system group '{group_name}': {}",
            out.stderr.trim()
        )));
    }
    logger.info(&format!("Created system group '{group_name}'"));
    on_event(SetupEvent::Completed(format!(
        "Created system group: {group_name}"
    )));
    Ok(())
}

// =============================================================================
/// Ensures `user_name` exists in `/etc/passwd`, creating it with `useradd --system`.
///
/// # Errors
/// Returns [`AppError`] if `useradd` fails or times out.
fn ensure_system_user(
    user_name: &str,
    group_name: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) -> AppResult<()> {
    if lookup_passwd_by_name(user_name).is_ok() {
        return Ok(());
    }
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    let args = [
        "--system",
        "--gid",
        group_name,
        "--home-dir",
        STATE_DIR,
        "--no-create-home",
        "--shell",
        SERVICE_USER_SHELL,
        user_name,
    ];
    let out = run_command_captured("useradd", &args, timeout)?;
    if !out.success {
        return Err(AppError::Setup(format!(
            "Failed creating system user '{user_name}': {}",
            out.stderr.trim()
        )));
    }
    logger.info(&format!(
        "Created system user '{user_name}' (group '{group_name}')"
    ));
    on_event(SetupEvent::Completed(format!(
        "Created system user: {user_name}"
    )));
    Ok(())
}

// =============================================================================
/// Appends `user_name` to any existing supplementary groups declared in constants.
fn add_supplementary_groups(
    user_name: &str,
    groups_csv: &str,
    logger: &Logger,
    on_event: &mut dyn FnMut(SetupEvent),
) {
    let timeout = Duration::from_secs(SUBPROCESS_TIMEOUT_SECS);
    for group in groups_csv.split_whitespace() {
        if lookup_group_gid(group).is_err() {
            on_event(SetupEvent::Warning(format!(
                "Supplementary group '{group}' not found (skipping)"
            )));
            continue;
        }
        if let Ok(out) = run_command_captured(
            "usermod",
            &["--append", "--groups", group, user_name],
            timeout,
        ) && out.success
        {
            logger.info(&format!("Added '{user_name}' to group '{group}'"));
        }
    }
}
