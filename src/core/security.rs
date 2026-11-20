//! Privilege checks, `SUDO_USER` identity resolution, and `CWE-59` atomic file I/O.
//!
//! Enforces OWASP ASVS v4.0 boundaries at every filesystem and user-identity edge:
//! symlink-free `O_NOFOLLOW` reads, atomic sibling-tempfile `fsync` + `rename` writes,
//! and thread-safe POSIX `passwd`/`group` resolution.

use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::constants::app_meta::NAME;
use crate::core::constants::service_defaults::{DIR_MODE, SUDO_TIMEOUT_SECS};
use crate::core::error::{AppError, AppResult};
use crate::core::process::run_command_inherited;

/// Buffer size in bytes for POSIX reentrant `getpwnam_r` / `getpwuid_r` / `getgrnam_r` lookups.
const PASSWD_BUF_LEN: usize = 4096;

/// Monotonic counter disambiguating concurrent atomic temporary files in the same process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// =============================================================================
/// Resolved POSIX account identity of a user on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    /// Login username from the system `passwd` database.
    pub username: String,
    /// Numeric POSIX user ID.
    pub uid: u32,
    /// Numeric POSIX primary group ID.
    pub gid: u32,
    /// Absolute home directory path (`pw_dir`).
    pub home_dir: PathBuf,
}

// =============================================================================
/// Returns `true` if the current process runs with effective UID `0` (`root`).
#[must_use]
pub fn is_root() -> bool {
    // SAFETY: `geteuid` is an async-signal-safe, stateless POSIX syscall with no preconditions.
    unsafe { libc::geteuid() == 0 }
}

// =============================================================================
/// Verifies that the process already holds `root` privileges.
///
/// # Errors
/// Returns [`AppError::Auth`] if effective UID is not `0`.
pub fn ensure_root_privileges() -> AppResult<()> {
    if is_root() {
        return Ok(());
    }
    Err(AppError::Auth(format!(
        "Root privileges are required (run via 'sudo ./setup.sh' or 'sudo {NAME} ...')"
    )))
}

// =============================================================================
/// Ensures administrative privileges are active, prompting via `sudo -v` if non-root.
///
/// # Errors
/// Returns [`AppError::Auth`] if `sudo -v` fails, is cancelled, or times out.
pub fn ensure_sudo_authenticated() -> AppResult<()> {
    if is_root() {
        return Ok(());
    }
    let timeout = Duration::from_secs(SUDO_TIMEOUT_SECS);
    let ok = run_command_inherited("sudo", &["-v"], timeout)
        .map_err(|err| AppError::Auth(format!("Failed to invoke sudo: {err}")))?;
    if !ok {
        return Err(AppError::Auth("sudo credential verification failed".into()));
    }
    Ok(())
}

// =============================================================================
/// Resolves the invoking desktop/CLI user's identity, honoring `SUDO_USER` when elevated.
///
/// # Errors
/// Returns [`AppError::Auth`] if the `passwd` record cannot be resolved.
pub fn resolve_caller_identity() -> AppResult<CallerIdentity> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        let trimmed = sudo_user.trim();
        if !trimmed.is_empty() && trimmed != "root" {
            return lookup_passwd_by_name(trimmed);
        }
    }
    // SAFETY: `geteuid` is stateless and always valid on POSIX targets.
    let euid = unsafe { libc::geteuid() };
    lookup_passwd_by_uid(euid)
}

// =============================================================================
/// Expands a leading `~` or `~/...` prefix against the invoking user's home directory.
///
/// # Errors
/// Returns [`AppError::Auth`] if `~` expansion is needed and user lookup fails.
pub fn expand_tilde(raw_path: &str) -> AppResult<PathBuf> {
    if raw_path == "~" {
        return Ok(resolve_caller_identity()?.home_dir);
    }
    if let Some(relative) = raw_path.strip_prefix("~/") {
        return Ok(resolve_caller_identity()?.home_dir.join(relative));
    }
    Ok(PathBuf::from(raw_path))
}

// =============================================================================
/// Queries the POSIX `passwd` database by username via thread-safe `getpwnam_r`.
///
/// # Errors
/// Returns [`AppError::Auth`] if the account does not exist or lookup fails.
pub fn lookup_passwd_by_name(username: &str) -> AppResult<CallerIdentity> {
    let c_name = CString::new(username)
        .map_err(|_| AppError::Auth(format!("Invalid username '{username}'")))?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result_ptr: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer: Vec<libc::c_char> = vec![0; PASSWD_BUF_LEN];

    // SAFETY: Pointers reference valid stack/heap storage outliving the call.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result_ptr,
        )
    };
    extract_passwd_identity(rc, result_ptr, &pwd, username)
}

// =============================================================================
/// Queries the POSIX `passwd` database by numeric UID via thread-safe `getpwuid_r`.
///
/// # Errors
/// Returns [`AppError::Auth`] if the UID has no `passwd` entry or lookup fails.
pub fn lookup_passwd_by_uid(uid: u32) -> AppResult<CallerIdentity> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result_ptr: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer: Vec<libc::c_char> = vec![0; PASSWD_BUF_LEN];

    // SAFETY: Pointers reference valid stack/heap storage outliving the call.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result_ptr,
        )
    };
    extract_passwd_identity(rc, result_ptr, &pwd, &uid.to_string())
}

// =============================================================================
/// Resolves a POSIX group name to its numeric GID via thread-safe `getgrnam_r`.
///
/// # Errors
/// Returns [`AppError::Auth`] if the group does not exist.
pub fn lookup_group_gid(group_name: &str) -> AppResult<u32> {
    let c_name = CString::new(group_name)
        .map_err(|_| AppError::Auth(format!("Invalid group name '{group_name}'")))?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result_ptr: *mut libc::group = std::ptr::null_mut();
    let mut buffer: Vec<libc::c_char> = vec![0; PASSWD_BUF_LEN];

    // SAFETY: Pointers reference valid stack/heap storage outliving `getgrnam_r`.
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &mut grp,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result_ptr,
        )
    };
    if rc != 0 || result_ptr.is_null() {
        return Err(AppError::Auth(format!("Group '{group_name}' not found")));
    }
    Ok(grp.gr_gid)
}

// =============================================================================
/// Converts a populated `libc::passwd` record into a safe [`CallerIdentity`].
///
/// # Errors
/// Returns [`AppError::Auth`] if the lookup status is non-zero or strings are missing.
fn extract_passwd_identity(
    rc: i32,
    result_ptr: *const libc::passwd,
    pwd: &libc::passwd,
    subject: &str,
) -> AppResult<CallerIdentity> {
    if rc != 0 || result_ptr.is_null() || pwd.pw_name.is_null() || pwd.pw_dir.is_null() {
        return Err(AppError::Auth(format!(
            "Failed to resolve user identity for '{subject}' (errno {rc})"
        )));
    }
    // SAFETY: `getpw*_r` succeeded and populated null-terminated strings inside `buffer`.
    let username = unsafe { CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .into_owned();
    let home_str = unsafe { CStr::from_ptr(pwd.pw_dir) }
        .to_string_lossy()
        .into_owned();
    if home_str.is_empty() {
        return Err(AppError::Auth(format!(
            "Resolved empty home directory for user '{username}'"
        )));
    }
    Ok(CallerIdentity {
        username,
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home_dir: PathBuf::from(home_str),
    })
}

// =============================================================================
/// Transitions `path` ownership to `user_name:group_name` when running as `root` (`CWE-59`-safe).
///
/// # Errors
/// Returns [`AppError`] if `path` is a symlink or `chown` fails for an existing account.
pub fn apply_ownership_if_root(path: &Path, user_name: &str, group_name: &str) -> AppResult<()> {
    if !is_root() || (user_name == "root" && group_name == "root") {
        return Ok(());
    }
    ensure_not_symlink(path)?;
    let uid = if user_name == "root" {
        0
    } else {
        lookup_passwd_by_name(user_name)?.uid
    };
    let gid = if group_name == "root" {
        0
    } else {
        lookup_group_gid(group_name)?
    };
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|err| {
        AppError::io(
            format!("Failed setting ownership on '{}'", path.display()),
            err,
        )
    })
}

// =============================================================================
/// Hands ownership of a user-facing file (such as `export -o`) back to `SUDO_USER` when root.
///
/// # Errors
/// Returns [`AppError`] if `path` is a symlink or `chown` fails.
pub fn apply_caller_ownership_if_sudo(path: &Path) -> AppResult<()> {
    if !is_root() {
        return Ok(());
    }
    ensure_not_symlink(path)?;
    if let Ok(caller) = resolve_caller_identity() {
        if caller.uid != 0 {
            std::os::unix::fs::chown(path, Some(caller.uid), Some(caller.gid)).map_err(|err| {
                AppError::io(
                    format!("Failed handing ownership on '{}'", path.display()),
                    err,
                )
            })?;
        }
    }
    Ok(())
}

// =============================================================================
/// Filesystem presence state distinguishing missing paths from `0750`/`0640` `EACCES` locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPresence {
    /// Path metadata is readable by the current effective UID/GID.
    Accessible,
    /// Path (or its `0750` parent directory) exists on disk, but traversal/stat is denied.
    PermissionDenied,
    /// Path does not exist (`ENOENT`).
    Missing,
}

// =============================================================================
/// Probes `path` without conflating `PermissionDenied` (`0750` parent) with `NotFound`.
#[must_use]
pub fn probe_path_presence(path: &Path) -> PathPresence {
    match fs::symlink_metadata(path) {
        Ok(_) => PathPresence::Accessible,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => PathPresence::PermissionDenied,
        Err(_) => PathPresence::Missing,
    }
}

// =============================================================================
/// Rejects `path` if it exists as a symbolic link (`CWE-59`).
///
/// # Errors
/// Returns [`AppError::Security`] if `path` is a symlink or [`AppError::Io`] on stat error.
pub fn ensure_not_symlink(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(AppError::Security(format!(
            "Symbolic link rejected (CWE-59): '{}'",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(AppError::io(
            format!("Failed inspecting metadata of '{}'", path.display()),
            err,
        )),
    }
}

// =============================================================================
/// Reads a UTF-8 text file safely using `O_NOFOLLOW` after symlink verification.
///
/// # Errors
/// Returns [`AppError::Security`] on symlink detection or [`AppError::Io`] on read failure.
pub fn read_file_nofollow(path: &Path) -> AppResult<String> {
    ensure_not_symlink(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| map_open_nofollow_err(path, err))?;

    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|err| AppError::io(format!("Failed reading '{}'", path.display()), err))?;
    Ok(content)
}

// =============================================================================
/// Writes `contents` atomically via a sibling `O_NOFOLLOW` tempfile, `fsync`, and `rename`.
///
/// # Errors
/// Returns [`AppError::Security`] if `path` is a symlink or [`AppError::Io`] on write error.
pub fn atomic_write_file(path: &Path, contents: &[u8], mode: u32) -> AppResult<()> {
    ensure_not_symlink(path)?;
    let parent = resolve_parent_dir(path)?;
    let tmp_path = build_atomic_temp_path(parent, path);

    let result = write_and_sync_temp_file(&tmp_path, contents, mode).and_then(|()| {
        fs::rename(&tmp_path, path).map_err(|err| {
            AppError::io(
                format!("Failed renaming temp file onto '{}'", path.display()),
                err,
            )
        })?;
        sync_directory_best_effort(parent);
        Ok(())
    });

    if result.is_err() {
        if let Err(_cleanup_err) = fs::remove_file(&tmp_path) {
            // Orphan cleanup is best-effort if `open` failed before creation.
        }
    }
    result
}

// =============================================================================
/// Maps `ELOOP` from `O_NOFOLLOW` to [`AppError::Security`] and other errors to [`AppError::Io`].
fn map_open_nofollow_err(path: &Path, err: std::io::Error) -> AppError {
    if err.raw_os_error() == Some(libc::ELOOP) {
        AppError::Security(format!(
            "Symbolic link rejected while opening '{}'",
            path.display()
        ))
    } else {
        AppError::io(format!("Failed opening '{}'", path.display()), err)
    }
}

// =============================================================================
/// Ensures the parent directory of `path` exists, is not a symlink, and has `DIR_MODE`.
///
/// # Errors
/// Returns [`AppError`] if `parent` is a symlink or directory creation fails.
fn resolve_parent_dir(path: &Path) -> AppResult<&Path> {
    let parent = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    ensure_not_symlink(parent)?;
    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|err| {
            AppError::io(
                format!("Failed creating parent directory '{}'", parent.display()),
                err,
            )
        })?;
        fs::set_permissions(parent, Permissions::from_mode(DIR_MODE)).map_err(|err| {
            AppError::io(
                format!("Failed setting mode on '{}'", parent.display()),
                err,
            )
        })?;
    }
    Ok(parent)
}

// =============================================================================
/// Generates a unique sibling temporary file path inside `parent` for atomic `rename`.
fn build_atomic_temp_path(parent: &Path, target: &Path) -> PathBuf {
    let base = target.file_name().and_then(|n| n.to_str()).unwrap_or(NAME);
    let pid = std::process::id();
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    parent.join(format!(".{base}.tmp.{pid}.{seq}.{nanos}"))
}

// =============================================================================
/// Creates `tmp_path` exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW`), writes, and syncs.
///
/// # Errors
/// Returns [`AppError`] if opening, writing, syncing, or `chmod` on `tmp_path` fails.
fn write_and_sync_temp_file(tmp_path: &Path, contents: &[u8], mode: u32) -> AppResult<()> {
    let mut tmp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(tmp_path)
        .map_err(|err| map_open_nofollow_err(tmp_path, err))?;

    tmp_file.write_all(contents).map_err(|err| {
        AppError::io(
            format!("Failed writing temporary file '{}'", tmp_path.display()),
            err,
        )
    })?;
    tmp_file.sync_all().map_err(|err| {
        AppError::io(
            format!("Failed syncing temporary file '{}'", tmp_path.display()),
            err,
        )
    })?;
    fs::set_permissions(tmp_path, Permissions::from_mode(mode)).map_err(|err| {
        AppError::io(
            format!("Failed setting permissions on '{}'", tmp_path.display()),
            err,
        )
    })
}

// =============================================================================
/// Flushes parent directory metadata after atomic rename so updates survive power loss.
fn sync_directory_best_effort(parent: &Path) {
    if let Ok(dir_handle) = File::open(parent) {
        if let Err(_sync_err) = dir_handle.sync_all() {
            // Virtual filesystems such as `tmpfs` may return `EINVAL` on directory `fsync`.
        }
    }
}
