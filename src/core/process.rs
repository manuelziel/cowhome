//! Safe, shell-free subprocess execution (`CWE-78`) and POSIX signal handling.
//!
//! Wraps [`std::process::Command`] so external system binaries always execute as
//! argument lists with bounded wall-clock timeouts, and provides async-signal-safe
//! `SIGINT`, `SIGTERM`, and `SIGHUP` flags for graceful daemon lifecycle management.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::core::constants::service_defaults::SUBPROCESS_POLL_MS;
use crate::core::error::{AppError, AppResult};

/// Async-signal-safe flag set upon receiving `SIGINT` (`Ctrl+C`) or `SIGTERM`.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe flag set upon receiving `SIGHUP` for live config hot-reload.
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Numeric POSIX signal code of the first termination signal (`SIGINT` / `SIGTERM`).
static TERMINATION_SIGNAL: AtomicI32 = AtomicI32::new(0);

// =============================================================================
/// Captured result of a completed non-interactive child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Numeric exit code (`None` if terminated by signal).
    pub exit_code: Option<i32>,
    /// `true` if the child exited cleanly with status `0`.
    pub success: bool,
    /// UTF-8 (lossy) standard output text.
    pub stdout: String,
    /// UTF-8 (lossy) standard error text.
    pub stderr: String,
}

// =============================================================================
/// Installs POSIX `SIGINT`, `SIGTERM`, and `SIGHUP` handlers without `SA_RESTART`.
pub fn install_signal_handlers() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    RELOAD_REQUESTED.store(false, Ordering::SeqCst);
    TERMINATION_SIGNAL.store(0, Ordering::SeqCst);
    // SAFETY: `sigaction` structs are zero-initialized, masks are cleared via `sigemptyset`,
    // and both C signal handlers perform exclusively lock-free atomic stores.
    unsafe {
        let mut term_sa: libc::sigaction = std::mem::zeroed();
        term_sa.sa_sigaction = handle_termination_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut term_sa.sa_mask);
        term_sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &term_sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &term_sa, std::ptr::null_mut());

        let mut hup_sa: libc::sigaction = std::mem::zeroed();
        hup_sa.sa_sigaction = handle_reload_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut hup_sa.sa_mask);
        hup_sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGHUP, &hup_sa, std::ptr::null_mut());
    }
}

// =============================================================================
/// Returns `true` if `SIGINT` or `SIGTERM` has been received.
#[must_use]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

// =============================================================================
/// Atomically reads and clears a pending `SIGHUP` configuration reload flag.
#[must_use]
pub fn take_reload_requested() -> bool {
    RELOAD_REQUESTED.swap(false, Ordering::SeqCst)
}

// =============================================================================
/// Returns the first received termination signal number (`0` if none).
#[must_use]
pub fn received_termination_signal() -> i32 {
    TERMINATION_SIGNAL.load(Ordering::SeqCst)
}

// =============================================================================
/// Sleeps up to `duration`, returning `true` early if shutdown or `SIGHUP` is signaled.
#[must_use]
pub fn wait_for_signal_or_timeout(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    let poll = Duration::from_millis(SUBPROCESS_POLL_MS);
    while Instant::now() < deadline {
        if is_shutdown_requested() || RELOAD_REQUESTED.load(Ordering::SeqCst) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(poll));
    }
    is_shutdown_requested() || RELOAD_REQUESTED.load(Ordering::SeqCst)
}

// =============================================================================
/// Async-signal-safe handler recording the first `SIGINT` / `SIGTERM` in atomic statics.
extern "C" fn handle_termination_signal(signum: libc::c_int) {
    if TERMINATION_SIGNAL
        .compare_exchange(0, signum, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Preserve the first termination signal code when multiple signals arrive.
    }
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

// =============================================================================
/// Async-signal-safe handler recording `SIGHUP` reload requests.
extern "C" fn handle_reload_signal(_signum: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

// =============================================================================
/// Executes `program` with `args` without a shell, capturing `stdout` and `stderr`.
///
/// # Errors
/// Returns [`AppError::Io`] on spawn/timeout failure or [`AppError::Cancelled`] on signal.
pub fn run_command_captured(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> AppResult<CommandOutput> {
    if is_shutdown_requested() {
        return Err(AppError::Cancelled);
    }
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| AppError::io(format!("Failed to execute '{program}'"), err))?;

    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let status = wait_with_timeout(&mut child, program, timeout)?;

    Ok(CommandOutput {
        exit_code: status.code(),
        success: status.success(),
        stdout: join_pipe_reader(stdout_reader),
        stderr: join_pipe_reader(stderr_reader),
    })
}

// =============================================================================
/// Executes `program` with inherited terminal streams (e.g., `sudo -v` or package managers).
///
/// # Errors
/// Returns [`AppError::Io`] on spawn/timeout failure or [`AppError::Cancelled`] on signal.
pub fn run_command_inherited(program: &str, args: &[&str], timeout: Duration) -> AppResult<bool> {
    if is_shutdown_requested() {
        return Err(AppError::Cancelled);
    }
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| AppError::io(format!("Failed to execute '{program}'"), err))?;

    let status = wait_with_timeout(&mut child, program, timeout)?;
    Ok(status.success())
}

// =============================================================================
/// Polls `child.try_wait()` until completion, signal interruption, or timeout expiry.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on signal or [`AppError::Io`] on wait/timeout error.
fn wait_with_timeout(child: &mut Child, program: &str, timeout: Duration) -> AppResult<ExitStatus> {
    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_millis(SUBPROCESS_POLL_MS);

    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| AppError::io(format!("Failed waiting on '{program}'"), err))?
        {
            return Ok(status);
        }
        if is_shutdown_requested() {
            terminate_timed_out_child(child);
            return Err(AppError::Cancelled);
        }
        if Instant::now() >= deadline {
            terminate_timed_out_child(child);
            return Err(AppError::io(
                format!(
                    "Subprocess '{program}' timed out after {}s",
                    timeout.as_secs()
                ),
                std::io::Error::new(std::io::ErrorKind::TimedOut, "subprocess timeout exceeded"),
            ));
        }
        thread::sleep(poll_interval);
    }
}

// =============================================================================
/// Sends `SIGKILL` to `child` and reaps its exit status to prevent zombie processes.
fn terminate_timed_out_child(child: &mut Child) {
    if child.kill().is_ok() {
        if let Err(_reap_err) = child.wait() {
            // Child already exited concurrently before reap completed.
        }
    }
}

// =============================================================================
/// Spawns a background reader thread to drain a child pipe without buffer deadlocks.
fn spawn_pipe_reader<R>(pipe: Option<R>) -> Option<thread::JoinHandle<String>>
where
    R: Read + Send + 'static,
{
    pipe.map(|mut reader| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            match reader.read_to_end(&mut bytes) {
                Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => String::new(),
            }
        })
    })
}

// =============================================================================
/// Joins a pipe-reader thread and returns its collected UTF-8 string.
fn join_pipe_reader(handle: Option<thread::JoinHandle<String>>) -> String {
    match handle {
        Some(join_handle) => join_handle.join().unwrap_or_default(),
        None => String::new(),
    }
}
