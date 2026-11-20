//! CowHome runtime stack builder & `SIGHUP` live-reconfiguration coordinator.
//!
//! Instantiates SQLite `Database`, Digital Twin `StateRegistry`, `RateLimiter`,
//! FritzBox `AHA` & `TR-064` clients, `WeatherService`, `FritzPollingWorker`, and
//! `RulesEngine` strictly within `Layer 4` (`services`) without upward `web` imports.

use std::sync::Arc;

use crate::core::config::AppConfig;
use crate::core::error::AppResult;
use crate::core::logger::Logger;
use crate::core::validation::is_api_key_secure;
use crate::db::Database;
use crate::models::registry::StateRegistry;
use crate::services::automation::RulesEngine;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client};
use crate::services::rate_limiter::RateLimiter;
use crate::services::weather::WeatherService;
use crate::services::worker::{FritzPollingWorker, PollingWorkerDeps};

// =============================================================================
/// Assembled domain runtime service graph for `cowhome daemon`.
#[derive(Debug)]
pub struct RuntimeStack {
    /// Pooled `WAL` SQLite database handle.
    pub db: Database,
    /// Shared in-memory Digital Twin state registry.
    pub registry: Arc<StateRegistry>,
    /// Sliding-window per-IP request and login `RateLimiter`.
    pub rate_limiter: Arc<RateLimiter>,
    /// Async FritzBox AHA Smart Home client.
    pub aha: Arc<FritzAhaClient>,
    /// Async FritzBox TR-064 SOAP router client.
    pub tr064: Arc<FritzTr064Client>,
    /// Async Open-Meteo weather client.
    pub weather_service: Arc<WeatherService>,
    /// Async FritzBox background polling worker.
    pub poller: Arc<FritzPollingWorker>,
    /// Async 5-tier Smart Automation `RulesEngine`.
    pub rules_engine: Arc<RulesEngine>,
}

impl RuntimeStack {
    /// Initializes all database, gateway, worker, and automation services from `config`.
    ///
    /// # Errors
    /// Returns [`crate::core::error::AppError`] if opening SQLite or building TLS HTTP clients fails.
    pub fn build(config: &AppConfig, logger: &Logger) -> AppResult<Self> {
        if !is_api_key_secure(&config.api.api_key) {
            logger.warning(
                "Insecure or placeholder API key configured! REST API requests will fail closed.",
            );
        }
        let db_path = config.resolved_database_path();
        let db = Database::open(&db_path, &config.weather.timezone, logger.clone())?;
        let registry = Arc::new(StateRegistry::new());
        let rate_limiter = Arc::new(RateLimiter::new(logger.clone()));
        let (aha, tr064, weather_service) = build_gateway_clients(config, &db, &registry, logger)?;

        let rules_engine = Arc::new(RulesEngine::new(
            config.automation.interval,
            db.clone(),
            Arc::clone(&registry),
            Arc::clone(&aha),
            Arc::clone(&tr064),
            Some(Arc::clone(&weather_service)),
            logger.clone(),
        ));

        let poller = Arc::new(FritzPollingWorker::new(PollingWorkerDeps {
            poll_interval_secs: config.fritzbox.poll_interval,
            rate_limit_window_secs: config.rate_limit.window,
            registry: Arc::clone(&registry),
            db: db.clone(),
            aha: Arc::clone(&aha),
            tr064: Arc::clone(&tr064),
            rate_limiter: Some(Arc::clone(&rate_limiter)),
            weather_service: Some(Arc::clone(&weather_service)),
            logger: logger.clone(),
        }));

        logger.info(&format!(
            "CowHome stack initialized (bind: {}:{}, fritzbox: {}, poll: {}s, rules: {}s)",
            config.server.host,
            config.server.port,
            config.fritzbox.host,
            config.fritzbox.poll_interval,
            config.automation.interval
        ));

        Ok(Self {
            db,
            registry,
            rate_limiter,
            aha,
            tr064,
            weather_service,
            poller,
            rules_engine,
        })
    }

    /// Hot-reloads credentials, intervals, coordinates, and rate limits on `SIGHUP`.
    pub async fn apply_reload(&self, new_cfg: &AppConfig, logger: &Logger) {
        if !is_api_key_secure(&new_cfg.api.api_key) {
            logger.warning("Reloaded configuration contains an insecure or placeholder API key!");
        }
        self.db.set_default_timezone(&new_cfg.weather.timezone);
        self.aha
            .reconfigure(
                &new_cfg.fritzbox.host,
                &new_cfg.fritzbox.username,
                &new_cfg.fritzbox.password,
            )
            .await;
        self.tr064
            .reconfigure(
                &new_cfg.fritzbox.host,
                &new_cfg.fritzbox.username,
                &new_cfg.fritzbox.password,
            )
            .await;
        self.weather_service.reconfigure(new_cfg.weather.clone());
        self.poller
            .reconfigure(new_cfg.fritzbox.poll_interval, new_cfg.rate_limit.window);
        self.rules_engine.reconfigure(new_cfg.automation.interval);

        logger.info(&format!(
            "Applied reloaded configuration (fritzbox: {}, poll: {}s, rules: {}s, tz: {})",
            new_cfg.fritzbox.host,
            new_cfg.fritzbox.poll_interval,
            new_cfg.automation.interval,
            new_cfg.weather.timezone
        ));
    }
}

// =============================================================================
/// Constructs `FritzAhaClient`, `FritzTr064Client`, and `WeatherService` handles.
///
/// # Errors
/// Returns [`crate::core::error::AppError`] if initializing any TLS HTTP client fails.
fn build_gateway_clients(
    config: &AppConfig,
    db: &Database,
    registry: &Arc<StateRegistry>,
    logger: &Logger,
) -> AppResult<(
    Arc<FritzAhaClient>,
    Arc<FritzTr064Client>,
    Arc<WeatherService>,
)> {
    let aha = Arc::new(FritzAhaClient::new(
        &config.fritzbox.host,
        &config.fritzbox.username,
        &config.fritzbox.password,
        logger.clone(),
    )?);
    let tr064 = Arc::new(FritzTr064Client::new(
        &config.fritzbox.host,
        &config.fritzbox.username,
        &config.fritzbox.password,
        logger.clone(),
    )?);
    let weather = Arc::new(WeatherService::new(
        config.weather.clone(),
        Some(db.clone()),
        Some(Arc::clone(registry)),
        logger.clone(),
    )?);
    Ok((aha, tr064, weather))
}
