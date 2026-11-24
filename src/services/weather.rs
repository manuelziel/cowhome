//! Async Open-Meteo outdoor climate client (`WeatherService`).
//!
//! Periodically fetches current weather conditions, 2-hour hourly preview slots, and
//! 5-day forecasts from `api.open-meteo.com/v1/forecast` with exponential retry backoff,
//! enriches observations with `WMO-No. 8` Magnus dew point and absolute humidity, updates
//! the Digital Twin (`StateRegistry`), and persists `WEATHER_01` in SQLite.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use chrono_tz::Tz;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::watch;

use crate::core::config::WeatherConfig;
use crate::core::constants::service_defaults::{
    DEFAULT_MAX_RETRIES, WORKER_MIN_SLEEP_SECS, WORKER_STARTUP_DELAY_SECS,
};
use crate::core::domain_constants::automation::weather_defaults::{
    BASE_URL, CURRENT_VARS, DAILY_VARS, HOURLY_FORECAST_SLOTS, HOURLY_STEP_HOURS, HOURLY_VARS,
    REQUEST_TIMEOUT_SECS, TIMEZONE, TIMEZONE_AUTO, UNKNOWN_WEATHER_CODE, UPDATE_INTERVAL_SECS,
    wmo_description,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::time_util::{now_epoch_f64, resolve_home_tz};
use crate::core::validation::sanitize_log_uri;
use crate::db::Database;
use crate::models::registry::StateRegistry;
use crate::models::weather::{
    CurrentWeather, DailyForecastSlot, HourlyForecastSlot, WeatherSnapshot,
};

// =============================================================================
/// Raw JSON payload returned by Open-Meteo `/v1/forecast`.
#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    /// IANA timezone string resolved by Open-Meteo when `timezone=auto`.
    timezone: Option<String>,
    /// Current weather observation block.
    current: Option<RawCurrentBlock>,
    /// Hourly arrays block.
    hourly: Option<RawHourlyBlock>,
    /// Daily arrays block.
    daily: Option<RawDailyBlock>,
}

// =============================================================================
/// Raw `current` block inside [`OpenMeteoResponse`].
#[derive(Debug, Default, Deserialize)]
struct RawCurrentBlock {
    /// 2-meter air temperature (`°C`).
    temperature_2m: Option<f64>,
    /// Apparent temperature (`°C`).
    apparent_temperature: Option<f64>,
    /// 2-meter relative humidity (`%`).
    relative_humidity_2m: Option<f64>,
    /// 10-meter wind speed (`km/h`).
    wind_speed_10m: Option<f64>,
    /// WMO weather condition code.
    weather_code: Option<i32>,
    /// Daylight indicator (`1` or `0`).
    is_day: Option<u8>,
}

// =============================================================================
/// Raw `hourly` parallel arrays inside [`OpenMeteoResponse`].
#[derive(Debug, Default, Deserialize)]
struct RawHourlyBlock {
    /// ISO local timestamps (`"YYYY-MM-DDTHH:MM"`).
    #[serde(default)]
    time: Vec<String>,
    /// Forecast temperatures (`°C`).
    #[serde(default)]
    temperature_2m: Vec<Option<f64>>,
    /// Forecast WMO weather codes.
    #[serde(default)]
    weather_code: Vec<Option<i32>>,
}

// =============================================================================
/// Raw `daily` parallel arrays inside [`OpenMeteoResponse`].
#[derive(Debug, Default, Deserialize)]
struct RawDailyBlock {
    /// ISO dates (`"YYYY-MM-DD"`).
    #[serde(default)]
    time: Vec<String>,
    /// Daily WMO weather codes.
    #[serde(default)]
    weather_code: Vec<Option<i32>>,
    /// Daily maximum temperatures (`°C`).
    #[serde(default)]
    temperature_2m_max: Vec<Option<f64>>,
    /// Daily minimum temperatures (`°C`).
    #[serde(default)]
    temperature_2m_min: Vec<Option<f64>>,
    /// Daily sunrise timestamps.
    #[serde(default)]
    sunrise: Vec<Option<String>>,
    /// Daily sunset timestamps.
    #[serde(default)]
    sunset: Vec<Option<String>>,
}

// =============================================================================
/// Async Open-Meteo weather service shared via `Arc<WeatherService>`.
#[derive(Debug)]
pub struct WeatherService {
    /// Pooled `rustls` HTTP client.
    http: Client,
    /// Hot-reloadable `[weather]` configuration.
    config: RwLock<WeatherConfig>,
    /// Latest cached [`WeatherSnapshot`].
    snapshot: RwLock<WeatherSnapshot>,
    /// Optional SQLite handle for `WEATHER_01` history persistence.
    db: Option<Database>,
    /// Optional Digital Twin registry updated after each successful fetch.
    registry: Option<Arc<StateRegistry>>,
    /// Application logger.
    logger: Logger,
}

impl WeatherService {
    /// Creates a new [`WeatherService`] initialized from `config`.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if building the `rustls` HTTP client fails.
    pub fn new(
        config: WeatherConfig,
        db: Option<Database>,
        registry: Option<Arc<StateRegistry>>,
        logger: Logger,
    ) -> AppResult<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| AppError::Service(format!("Failed building Weather HTTP client: {e}")))?;
        let init_tz = initial_timezone_name(&config.timezone, &logger);
        let snap = WeatherSnapshot {
            status: String::from("unknown"),
            timezone: init_tz,
            last_update: None,
            current: None,
            hourly: Vec::new(),
            daily: Vec::new(),
        };
        Ok(Self {
            http,
            config: RwLock::new(config),
            snapshot: RwLock::new(snap),
            db,
            registry,
            logger,
        })
    }

    /// Applies updated `[weather]` settings on `SIGHUP` reload.
    pub fn reconfigure(&self, new_cfg: WeatherConfig) {
        let resolved_tz = initial_timezone_name(&new_cfg.timezone, &self.logger);
        if let Ok(mut cfg) = self.config.write() {
            *cfg = new_cfg;
        }
        if let Ok(mut snap) = self.snapshot.write() {
            snap.timezone = resolved_tz;
        }
    }

    /// Returns a cloned copy of the latest [`WeatherSnapshot`].
    #[must_use]
    pub fn get_snapshot(&self) -> WeatherSnapshot {
        self.snapshot.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Returns a cloned copy of the latest [`CurrentWeather`], if available.
    #[must_use]
    pub fn get_current_weather(&self) -> Option<CurrentWeather> {
        self.snapshot.read().ok().and_then(|g| g.current.clone())
    }

    /// Returns the operational status token (`"online"`, `"loading"`, `"error"`, `"unknown"`).
    #[must_use]
    pub fn get_status(&self) -> String {
        self.snapshot
            .read()
            .map(|g| g.status.clone())
            .unwrap_or_else(|_| String::from("unknown"))
    }

    /// Returns the resolved home IANA timezone name (`"Europe/Berlin"`).
    #[must_use]
    pub fn get_timezone_name(&self) -> String {
        self.snapshot
            .read()
            .map(|g| g.timezone.clone())
            .unwrap_or_else(|_| TIMEZONE.to_owned())
    }

    /// Resolves the home [`Tz`] instance for wall-clock schedule evaluation.
    #[must_use]
    pub fn get_tz(&self) -> Tz {
        let name = self.get_timezone_name();
        resolve_home_tz(Some(&name))
    }

    /// Runs the periodic background Open-Meteo fetch loop until `shutdown_rx` signals `true`.
    pub async fn run_loop(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        self.set_status("loading");
        self.logger.info("WeatherService started");
        if wait_or_stop(
            &mut shutdown_rx,
            Duration::from_secs(WORKER_STARTUP_DELAY_SECS),
        )
        .await
        {
            self.logger.info("WeatherService stopped");
            return;
        }
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let started = Instant::now();
            if let Err(err) = self.fetch_once().await {
                self.set_status("error");
                self.logger.error(&format!(
                    "Weather fetch error: {}",
                    sanitize_log_uri(&err.to_string())
                ));
            }
            let interval = self
                .config
                .read()
                .map_or(UPDATE_INTERVAL_SECS, |c| c.update_interval);
            let elapsed = started.elapsed().as_secs();
            let sleep_secs = interval.saturating_sub(elapsed).max(WORKER_MIN_SLEEP_SECS);
            if wait_or_stop(&mut shutdown_rx, Duration::from_secs(sleep_secs)).await {
                break;
            }
        }
        self.logger.info("WeatherService stopped");
    }

    /// Fetches Open-Meteo once (with exponential retry backoff), updates caches, and persists `WEATHER_01`.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if all retry attempts fail or JSON decoding fails.
    pub async fn fetch_once(&self) -> AppResult<WeatherSnapshot> {
        let cfg = self
            .config
            .read()
            .map(|g| g.clone())
            .map_err(|_| AppError::Service("Weather config lock poisoned".into()))?;
        let url = build_open_meteo_url(&cfg);
        let raw = self.get_with_retries(&url).await?;
        let now_ts = now_epoch_f64();
        let next_tz = self.resolve_effective_tz(&cfg.timezone, raw.timezone.as_deref());
        let tz_obj = resolve_home_tz(Some(&next_tz));

        let current = raw.current.map(parse_current_block);
        let hourly = raw
            .hourly
            .map(|h| parse_hourly_block(&h, tz_obj))
            .unwrap_or_default();
        let daily = raw.daily.map(parse_daily_block).unwrap_or_default();

        let snap = WeatherSnapshot {
            status: String::from("online"),
            timezone: next_tz.clone(),
            last_update: Some(now_ts),
            current,
            hourly,
            daily,
        };
        if let Ok(mut guard) = self.snapshot.write() {
            *guard = snap.clone();
        }
        if let Some(ref reg) = self.registry {
            reg.set_weather(Some(snap.clone()));
        }
        if let Some(ref db) = self.db {
            db.set_default_timezone(&next_tz);
            if let Some(hist_val) = snap.to_history_json()
                && let Err(err) = db
                    .run_blocking(move |d| d.persist_weather_history(&hist_val, Some(now_ts)))
                    .await
            {
                self.logger
                    .error(&format!("Failed to persist weather history: {err}"));
            }
        }
        self.logger.debug("Weather data updated successfully");
        Ok(snap)
    }

    /// Performs a `GET` request with up to `DEFAULT_MAX_RETRIES` exponential backoff retries.
    async fn get_with_retries(&self, url: &str) -> AppResult<OpenMeteoResponse> {
        let mut last_err = String::from("uninitialized");
        for attempt in 0..=DEFAULT_MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_secs(1_u64 << (attempt - 1).min(4));
                tokio::time::sleep(delay).await;
            }
            match self.http.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    return resp.json::<OpenMeteoResponse>().await.map_err(|e| {
                        AppError::Service(format!("Open-Meteo JSON parse failed: {e}"))
                    });
                }
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    last_err = format!("HTTP {code}");
                    if !matches!(code, 429 | 500 | 502 | 503 | 504) {
                        break;
                    }
                    if let Some(ra) = parse_retry_after_secs(&resp) {
                        tokio::time::sleep(Duration::from_secs(ra.min(30))).await;
                    }
                }
                Err(err) => {
                    last_err = sanitize_log_uri(&err.to_string());
                }
            }
        }
        Err(AppError::Service(format!(
            "Failed to fetch weather data ({last_err})"
        )))
    }

    /// Updates `snapshot.status` atomically.
    fn set_status(&self, status: &str) {
        if let Ok(mut guard) = self.snapshot.write() {
            guard.status = status.to_owned();
        }
    }

    /// Updates the home IANA timezone from Open-Meteo when `config.timezone` is `"auto"`.
    fn resolve_effective_tz(&self, configured_tz: &str, api_tz: Option<&str>) -> String {
        let cfg_trim = configured_tz.trim();
        let allow_auto = cfg_trim.is_empty()
            || cfg_trim.eq_ignore_ascii_case(TIMEZONE_AUTO)
            || cfg_trim.eq_ignore_ascii_case(TIMEZONE);
        if allow_auto
            && let Some(cand) = api_tz.map(str::trim).filter(|s| s.contains('/'))
            && cand.parse::<Tz>().is_ok()
        {
            return cand.to_owned();
        }
        self.get_timezone_name()
    }
}

// =============================================================================
/// Resolves initial configured timezone string with fallback to `Europe/Berlin`.
fn initial_timezone_name(raw_tz: &str, logger: &Logger) -> String {
    let trimmed = raw_tz.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(TIMEZONE_AUTO) {
        return TIMEZONE.to_owned();
    }
    if trimmed.parse::<Tz>().is_ok() {
        trimmed.to_owned()
    } else {
        logger.warning(&format!(
            "Invalid configured timezone '{trimmed}', falling back to {TIMEZONE}"
        ));
        TIMEZONE.to_owned()
    }
}

// =============================================================================
/// Builds the Open-Meteo `/forecast` request URL for `cfg`.
fn build_open_meteo_url(cfg: &WeatherConfig) -> String {
    format!(
        "{BASE_URL}/forecast?latitude={}&longitude={}&current={CURRENT_VARS}\
         &hourly={HOURLY_VARS}&daily={DAILY_VARS}&timezone=auto&forecast_days={}",
        cfg.latitude, cfg.longitude, cfg.forecast_days
    )
}

// =============================================================================
/// Converts [`RawCurrentBlock`] into psychrometrically enriched [`CurrentWeather`].
fn parse_current_block(raw: RawCurrentBlock) -> CurrentWeather {
    let code = raw.weather_code.unwrap_or(UNKNOWN_WEATHER_CODE);
    let mut cur = CurrentWeather {
        temperature: raw.temperature_2m,
        apparent_temperature: raw.apparent_temperature,
        humidity: raw.relative_humidity_2m,
        dew_point_c: None,
        abs_humidity_gm3: None,
        wind_speed: raw.wind_speed_10m,
        weather_code: code,
        weather_description: wmo_description(code).to_owned(),
        is_day: raw.is_day.unwrap_or(1),
    };
    cur.enrich_psychrometrics();
    cur
}

// =============================================================================
/// Samples up to `HOURLY_FORECAST_SLOTS` 2-hour preview slots starting from current local hour.
fn parse_hourly_block(raw: &RawHourlyBlock, tz: Tz) -> Vec<HourlyForecastSlot> {
    if raw.time.is_empty() {
        return Vec::new();
    }
    let current_iso = Utc::now()
        .with_timezone(&tz)
        .format("%Y-%m-%dT%H")
        .to_string();
    let start_idx = raw
        .time
        .iter()
        .position(|t| t.as_str() >= current_iso.as_str())
        .unwrap_or(0);

    let mut slots = Vec::with_capacity(HOURLY_FORECAST_SLOTS);
    for step in 0..HOURLY_FORECAST_SLOTS {
        let idx = start_idx + step * HOURLY_STEP_HOURS;
        let Some(ts) = raw.time.get(idx) else {
            break;
        };
        let code = raw
            .weather_code
            .get(idx)
            .and_then(|&c| c)
            .unwrap_or(UNKNOWN_WEATHER_CODE);
        let time_label = ts
            .rsplit_once('T')
            .map_or_else(|| ts.clone(), |(_, hm)| hm.to_owned());
        slots.push(HourlyForecastSlot {
            time: time_label,
            temperature: raw.temperature_2m.get(idx).and_then(|&t| t),
            weather_code: code,
            weather_description: wmo_description(code).to_owned(),
        });
    }
    slots
}

// =============================================================================
/// Converts [`RawDailyBlock`] parallel arrays into [`DailyForecastSlot`] entries.
fn parse_daily_block(raw: RawDailyBlock) -> Vec<DailyForecastSlot> {
    let mut out = Vec::with_capacity(raw.time.len());
    for (idx, date) in raw.time.into_iter().enumerate() {
        let code = raw
            .weather_code
            .get(idx)
            .and_then(|&c| c)
            .unwrap_or(UNKNOWN_WEATHER_CODE);
        out.push(DailyForecastSlot {
            date,
            weather_code: code,
            weather_description: wmo_description(code).to_owned(),
            temp_max: raw.temperature_2m_max.get(idx).and_then(|&v| v),
            temp_min: raw.temperature_2m_min.get(idx).and_then(|&v| v),
            sunrise: raw.sunrise.get(idx).and_then(Clone::clone),
            sunset: raw.sunset.get(idx).and_then(Clone::clone),
        });
    }
    out
}

// =============================================================================
/// Extracts an integer `Retry-After` header duration in seconds, if present.
fn parse_retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

// =============================================================================
/// Sleeps for `duration` or returns `true` immediately when `shutdown_rx` signals shutdown.
async fn wait_or_stop(shutdown_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => *shutdown_rx.borrow(),
        res = shutdown_rx.changed() => res.is_err() || *shutdown_rx.borrow(),
    }
}
