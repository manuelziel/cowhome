//! Shared `axum` application dependency-injection state (`Arc<AppState>`).
//!
//! Holds hot-reloadable configuration, SQLite `Database`, Digital Twin `StateRegistry`,
//! FritzBox `AHA` & `TR-064` clients, `WeatherService`, `RulesEngine`, and `RateLimiter`.

use std::sync::{Arc, RwLock};

use crate::core::config::AppConfig;
use crate::core::domain_constants::automation::weather_defaults::{TIMEZONE, TIMEZONE_AUTO};
use crate::core::heating_forecast::ForecastDayInput;
use crate::core::logger::Logger;
use crate::db::Database;
use crate::models::registry::StateRegistry;
use crate::services::automation::RulesEngine;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client};
use crate::services::rate_limiter::RateLimiter;
use crate::services::weather::WeatherService;

// =============================================================================
/// Dependency-injection container shared across all `axum` middlewares and handlers.
#[derive(Debug)]
pub struct AppState {
    /// Hot-reloadable application configuration (`SIGHUP`-safe).
    pub config: RwLock<AppConfig>,
    /// Pooled `WAL` SQLite database handle.
    pub db: Database,
    /// In-memory Digital Twin state registry.
    pub registry: Arc<StateRegistry>,
    /// Async FritzBox AHA Smart Home client.
    pub aha: Arc<FritzAhaClient>,
    /// Async FritzBox TR-064 SOAP router client.
    pub tr064: Arc<FritzTr064Client>,
    /// Optional async Open-Meteo weather client.
    pub weather_service: Option<Arc<WeatherService>>,
    /// Optional async `RulesEngine` background evaluator.
    pub rules_engine: Option<Arc<RulesEngine>>,
    /// Monotonic sliding-window IP & brute-force rate limiter.
    pub rate_limiter: Arc<RateLimiter>,
    /// Application logger.
    pub logger: Logger,
}

impl AppState {
    /// Creates a new [`AppState`] instance wrapped in [`Arc`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AppConfig,
        db: Database,
        registry: Arc<StateRegistry>,
        aha: Arc<FritzAhaClient>,
        tr064: Arc<FritzTr064Client>,
        weather_service: Option<Arc<WeatherService>>,
        rules_engine: Option<Arc<RulesEngine>>,
        rate_limiter: Arc<RateLimiter>,
        logger: Logger,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            db,
            registry,
            aha,
            tr064,
            weather_service,
            rules_engine,
            rate_limiter,
            logger,
        })
    }

    /// Clones the active [`AppConfig`] snapshot for request validation and rate limiting.
    #[must_use]
    pub fn config_snapshot(&self) -> AppConfig {
        self.config.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Replaces the active [`AppConfig`] snapshot during live `SIGHUP` configuration reload.
    pub fn reconfigure(&self, next: AppConfig) {
        if let Ok(mut guard) = self.config.write() {
            *guard = next;
        }
    }

    /// Resolves the home IANA timezone name from `WeatherService` or `config.weather.timezone`.
    #[must_use]
    pub fn home_tz_name(&self) -> String {
        if let Some(ref ws) = self.weather_service {
            let tz = ws.get_timezone_name();
            if !tz.is_empty() {
                return tz;
            }
        }
        let cfg = self.config_snapshot();
        let raw = cfg.weather.timezone.trim();
        if !raw.is_empty() && !raw.eq_ignore_ascii_case(TIMEZONE_AUTO) {
            return raw.to_owned();
        }
        TIMEZONE.to_owned()
    }

    /// Extracts `(5_day_daily_forecast, live_outdoor_temp_c)` from `WeatherService`.
    #[must_use]
    pub fn forecast_and_outdoor(&self) -> (Vec<ForecastDayInput>, Option<f64>) {
        let Some(ref ws) = self.weather_service else {
            return (Vec::new(), None);
        };
        let snap = ws.get_snapshot();
        let out_t = snap.current.as_ref().and_then(|c| c.temperature);
        (snap.to_forecast_inputs(), out_t)
    }
}
