//! Async foreground `systemd` (`Type=simple`) daemon supervisor (`cowhome daemon`).
//!
//! Sits in `crate::web` (`Layer 5`) above `crate::services` (`Layer 4`): constructs
//! [`RuntimeStack`] + [`AppState`], binds the `axum` REST API server, launches background
//! workers, applies live `SIGHUP` reloads, and drains all tasks cleanly on `SIGINT`/`SIGTERM`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::router::build_router;
use super::state::AppState;
use crate::core::config::{load_runtime_config, AppConfig};
use crate::core::constants::{
    app_meta::{SOFTWARE_NAME, VERSION},
    cli_style::{EXIT_SIGINT, EXIT_SUCCESS},
    service_defaults::SERVICE_TIMEOUT_STOP_SEC,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::{LogLevel, Logger};
use crate::services::application::RuntimeStack;

// =============================================================================
/// Runs the full CowHome daemon in the foreground until `SIGINT` or `SIGTERM`.
///
/// # Errors
/// Returns [`AppError`] if creating the `tokio` runtime, opening SQLite, or binding
/// `server.host:server.port` fails.
pub fn run_daemon_supervisor(
    initial_config: AppConfig,
    explicit_cfg_path: Option<&Path>,
    debug_override: bool,
    logger: Logger,
) -> AppResult<i32> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| AppError::Service(format!("Failed initializing tokio runtime: {err}")))?;

    let cfg_path_buf = explicit_cfg_path.map(Path::to_path_buf);
    rt.block_on(run_async_daemon(
        initial_config,
        cfg_path_buf.as_deref(),
        debug_override,
        logger,
    ))
}

// =============================================================================
/// Async supervisor spawning HTTP server + background workers and awaiting OS signals.
///
/// # Errors
/// Returns [`AppError`] if initializing [`RuntimeStack`], binding TCP, or registering signals fails.
async fn run_async_daemon(
    initial_config: AppConfig,
    explicit_cfg_path: Option<&Path>,
    debug_override: bool,
    logger: Logger,
) -> AppResult<i32> {
    logger.info(&format!(
        "{SOFTWARE_NAME} v{VERSION} daemon starting in foreground"
    ));

    let stack = RuntimeStack::build(&initial_config, &logger)?;
    let web_state = build_web_state(&initial_config, &stack, &logger);
    let bind_addr = format!(
        "{}:{}",
        initial_config.server.host, initial_config.server.port
    );
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|err| AppError::io(format!("Failed binding HTTP server on '{bind_addr}'"), err))?;
    logger.info(&format!("REST API listening on http://{bind_addr}"));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http_handle = spawn_http_server(
        listener,
        Arc::clone(&web_state),
        shutdown_rx.clone(),
        &logger,
    );
    let worker_handles = spawn_background_workers(&initial_config, &stack, &shutdown_rx, &logger);

    let exit_code = supervise_os_signals(
        &stack,
        &web_state,
        explicit_cfg_path,
        debug_override,
        &logger,
    )
    .await?;

    drain_daemon_tasks(shutdown_tx, http_handle, worker_handles, &logger).await;
    stack.db.wal_checkpoint();
    stack.db.close();
    logger.info(&format!("{SOFTWARE_NAME} daemon stopped cleanly."));
    Ok(exit_code)
}

// =============================================================================
/// Wires [`AppState`] from [`RuntimeStack`] handles without cyclic upward imports.
fn build_web_state(config: &AppConfig, stack: &RuntimeStack, logger: &Logger) -> Arc<AppState> {
    AppState::new(
        config.clone(),
        stack.db.clone(),
        Arc::clone(&stack.registry),
        Arc::clone(&stack.aha),
        Arc::clone(&stack.tr064),
        Some(Arc::clone(&stack.weather_service)),
        Some(Arc::clone(&stack.rules_engine)),
        Arc::clone(&stack.rate_limiter),
        logger.clone(),
    )
}

// =============================================================================
/// Spawns the `axum` HTTP server task with graceful shutdown wired to `shutdown_rx`.
fn spawn_http_server(
    listener: TcpListener,
    web_state: Arc<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
    logger: &Logger,
) -> JoinHandle<()> {
    let router = build_router(web_state);
    let http_logger = logger.clone();
    tokio::spawn(async move {
        let make_svc = router.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum::serve(listener, make_svc).with_graceful_shutdown(async move {
            if shutdown_rx.wait_for(|&stop| stop).await.is_err() {
                // Shutdown sender dropped; proceed with graceful HTTP drain.
            }
        });
        if let Err(err) = server.await {
            http_logger.error(&format!("HTTP server error: {err}"));
        }
    })
}

// =============================================================================
/// Spawns `WeatherService`, `FritzPollingWorker`, and `RulesEngine` when `daemon.enabled`.
fn spawn_background_workers(
    config: &AppConfig,
    stack: &RuntimeStack,
    shutdown_rx: &watch::Receiver<bool>,
    logger: &Logger,
) -> Vec<JoinHandle<()>> {
    if !config.daemon.enabled {
        logger.warning("Background polling & automation disabled (daemon.enabled = false).");
        return Vec::new();
    }
    let ws = Arc::clone(&stack.weather_service);
    let ws_rx = shutdown_rx.clone();
    let poller = Arc::clone(&stack.poller);
    let poll_rx = shutdown_rx.clone();
    let engine = Arc::clone(&stack.rules_engine);
    let eng_rx = shutdown_rx.clone();

    vec![
        tokio::spawn(async move { ws.run_loop(ws_rx).await }),
        tokio::spawn(async move { poller.run_loop(poll_rx).await }),
        tokio::spawn(async move { engine.run_loop(eng_rx).await }),
    ]
}

// =============================================================================
/// Signals graceful shutdown and awaits HTTP + worker tasks within `SERVICE_TIMEOUT_STOP_SEC`.
async fn drain_daemon_tasks(
    shutdown_tx: watch::Sender<bool>,
    http_handle: JoinHandle<()>,
    worker_handles: Vec<JoinHandle<()>>,
    logger: &Logger,
) {
    logger.info("Initiating graceful shutdown of HTTP server and background workers...");
    if shutdown_tx.send(true).is_err() {
        logger.debug("All shutdown receivers had already closed.");
    }
    let stop_timeout = Duration::from_secs(SERVICE_TIMEOUT_STOP_SEC.saturating_sub(2).max(3));
    let drain_res = tokio::time::timeout(stop_timeout, async {
        if let Err(err) = http_handle.await {
            logger.warning(&format!(
                "HTTP server task join error during shutdown: {err}"
            ));
        }
        for handle in worker_handles {
            if let Err(err) = handle.await {
                logger.warning(&format!(
                    "Background worker join error during shutdown: {err}"
                ));
            }
        }
    })
    .await;
    if drain_res.is_err() {
        logger.warning("Graceful shutdown timed out; forcing remaining tasks to close.");
    }
}

// =============================================================================
/// Awaits `SIGINT`, `SIGTERM`, and `SIGHUP` via `tokio::signal::unix`.
///
/// # Errors
/// Returns [`AppError::Io`] if registering POSIX signal listeners fails.
async fn supervise_os_signals(
    stack: &RuntimeStack,
    web_state: &AppState,
    explicit_cfg_path: Option<&Path>,
    debug_override: bool,
    logger: &Logger,
) -> AppResult<i32> {
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| AppError::io("Failed registering SIGINT listener", e))?;
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| AppError::io("Failed registering SIGTERM listener", e))?;
    let mut sighup = signal(SignalKind::hangup())
        .map_err(|e| AppError::io("Failed registering SIGHUP listener", e))?;

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                logger.info("Received SIGINT; shutting down...");
                return Ok(EXIT_SIGINT);
            }
            _ = sigterm.recv() => {
                logger.info("Received SIGTERM; shutting down...");
                return Ok(EXIT_SUCCESS);
            }
            _ = sighup.recv() => {
                handle_sighup_reload(stack, web_state, explicit_cfg_path, debug_override, logger).await;
            }
        }
    }
}

// =============================================================================
/// Reloads `config.toml` and hot-reconfigures `Logger`, `RuntimeStack`, & `AppState` on `SIGHUP`.
async fn handle_sighup_reload(
    stack: &RuntimeStack,
    web_state: &AppState,
    explicit_cfg_path: Option<&Path>,
    debug_override: bool,
    logger: &Logger,
) {
    logger.info("Received SIGHUP; reloading runtime configuration...");
    match load_runtime_config(explicit_cfg_path) {
        Ok((new_cfg, path)) => {
            let lvl = if debug_override {
                LogLevel::Debug
            } else {
                new_cfg.debug.level
            };
            logger.reconfigure(
                lvl,
                new_cfg.debug.console_colors,
                &[&new_cfg.api.api_key, &new_cfg.fritzbox.password],
            );
            stack.apply_reload(&new_cfg, logger).await;
            web_state.reconfigure(new_cfg);
            logger.info(&format!("Reloaded configuration from '{}'", path.display()));
        }
        Err(err) => {
            logger.error(&format!("Failed reloading configuration on SIGHUP: {err}"));
        }
    }
}
