//! 5-Day Heating Outlook and Forecast Calculator (`VDI 2067` / `VDI 3807` / `ISO 15927-6`).
//!
//! Combines the 5-day Open-Meteo outdoor forecast, active weekly `HeatingSchedule` slots,
//! `VacationMode` / `WeatherSmartHeating` overrides, and the learned building thermal
//! demand signature (`min / (K * d)`) to project daily heating runtime and savings potential.

mod schedule_eval;

use std::collections::HashMap;

use chrono::{Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use schedule_eval::resolve_day_target;
pub use schedule_eval::{canonical_weekday_key, is_date_within_window, parse_hhmm_minutes};

use crate::core::domain_constants::{
    analytics::efficiency_defaults::{
        FROST_IDLE_MAX_TEMP, IDEAL_DEMAND_INTENSITY, MAX_LEARNED_INTENSITY, MAX_SAVINGS_PERCENT,
        MINUTES_PER_DAY, MIN_ACTIVE_DEMAND_MINUTES, MIN_BASELINE_DEMAND_MINUTES,
        MIN_LEARNED_INTENSITY, RECOMMENDED_COMFORT_HOURS, RECOMMENDED_COMFORT_TEMP,
        RECOMMENDED_ECO_HOURS, RECOMMENDED_ECO_TEMP, TREND_DEADBAND_MINUTES,
        TREND_DEADBAND_PERCENT, TREND_FALLING, TREND_MAX_PERCENT, TREND_MIN_PERCENT, TREND_RISING,
        TREND_STABLE, VDI_SAVINGS_PER_KELVIN,
    },
    automation::{
        automation_identifiers::{HEATING_SCHEDULE, WEEKDAYS},
        weather_defaults::FORECAST_DAYS,
    },
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::now_epoch_f64;

/// Default forecast horizon in days (`FORECAST_DAYS` widened to `usize`).
const FORECAST_DAYS_DEFAULT: usize = FORECAST_DAYS as usize;

// =============================================================================
/// Raw daily outdoor weather forecast entry supplied to [`build_device_forecast`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastDayInput {
    /// ISO date string (`"YYYY-MM-DD"`).
    pub date: String,
    /// Daily minimum outdoor temperature (`°C`).
    pub temp_min: Option<f64>,
    /// Daily maximum outdoor temperature (`°C`).
    pub temp_max: Option<f64>,
}

// =============================================================================
/// Snapshot of a single automation feature (`is_active` + JSON `config`) for simulation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RuleSnapshot {
    /// Whether the feature is actively enabled in the evaluated scope.
    pub is_active: bool,
    /// Raw JSON configuration object for the feature.
    pub config: Value,
}

// =============================================================================
/// Input bundle for computing a single thermostat's 5-day heating outlook.
#[derive(Debug, Clone)]
pub struct DeviceForecastRequest<'a> {
    /// Thermostat AIN.
    pub ain: &'a str,
    /// Optional software group identifier (`"group-1"`).
    pub group_id: Option<&'a str>,
    /// Authoritative automation rules mapped by feature name.
    pub rules: &'a HashMap<String, RuleSnapshot>,
    /// Optional house-wide `VacationMode` rule snapshot.
    pub global_vacation: Option<&'a RuleSnapshot>,
    /// Current thermostat target temperature (`°C`).
    pub current_target: f64,
    /// Configured comfort preset temperature (`°C`).
    pub comfort_temp: f64,
    /// Configured eco setback temperature (`°C`).
    pub eco_temp: f64,
    /// Learned specific demand intensity (`min / (K * d)`) from 7–14d history.
    pub learned_intensity: f64,
    /// Historical 7-day baseline daily heating demand (`min/d`).
    pub baseline_daily_minutes: f64,
    /// Fallback outdoor temperature (`°C`) when Open-Meteo daily forecast is empty.
    pub fallback_outdoor_temp: f64,
    /// Optional 5-day Open-Meteo daily forecast list.
    pub daily_forecast: &'a [ForecastDayInput],
    /// Optional evaluation timestamp override (seconds).
    pub now_ts: Option<f64>,
}

// =============================================================================
/// Standardized 5-day heating outlook returned on `/api/v1/state` and `/api/v1/history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeatingForecast {
    /// Projected demand trend (`"rising"`, `"falling"`, or `"stable"`).
    pub trend_direction: String,
    /// Clamped percentage change vs 7-day baseline (`-100.0`..=`200.0`).
    pub trend_delta_percent: f64,
    /// Projected average daily heating minutes over the next 5 days.
    pub projected_daily_heating_minutes: f64,
    /// Estimated additional `VDI 2067` schedule savings potential (`0.0`..=`30.0 %`).
    pub potential_schedule_savings_percent: f64,
    /// Historical 7-day baseline daily heating minutes.
    pub baseline_daily_heating_minutes: f64,
    /// Mean forecast outdoor temperature (`°C`) across the 5-day horizon.
    pub avg_forecast_outdoor_temp: f64,
    /// Mean time-weighted scheduled target temperature (`°C`) across the 5-day horizon.
    pub avg_scheduled_target_temp: f64,
    /// Number of forecast days evaluated (`5`).
    pub forecast_days: usize,
}

// =============================================================================
/// Normalized internal forecast day used during HDD simulation.
#[derive(Debug, Clone)]
struct NormalizedDay {
    /// Canonical weekday key (`"mon"`..`"sun"`).
    pub weekday: &'static str,
    /// `"MM-DD"` date token for seasonal window matching.
    pub month_day: String,
    /// UTC midnight UNIX epoch (`f64`).
    pub day_ts: f64,
    /// Daily maximum outdoor temperature (`°C`).
    pub temp_max: f64,
    /// Daily mean outdoor temperature (`°C`).
    pub temp_mean: f64,
}

// =============================================================================
/// Computes trend direction and clamped percentage delta against the 7-day baseline.
#[must_use]
pub fn compute_heating_trend(projected_minutes: f64, baseline_minutes: f64) -> (&'static str, f64) {
    if projected_minutes < MIN_ACTIVE_DEMAND_MINUTES && baseline_minutes < MIN_ACTIVE_DEMAND_MINUTES
    {
        return (TREND_STABLE, 0.0);
    }
    let delta_min = projected_minutes - baseline_minutes;
    let denom = baseline_minutes.max(MIN_BASELINE_DEMAND_MINUTES);
    let raw_pct = (delta_min / denom) * 100.0;
    let clamped = round_decimals(raw_pct.clamp(TREND_MIN_PERCENT, TREND_MAX_PERCENT), 1);
    if clamped.abs() < TREND_DEADBAND_PERCENT || delta_min.abs() < TREND_DEADBAND_MINUTES {
        return (TREND_STABLE, clamped);
    }
    let dir = if clamped > 0.0 {
        TREND_RISING
    } else {
        TREND_FALLING
    };
    (dir, clamped)
}

// =============================================================================
/// Estimates additional `VDI 2067` schedule savings (`0.0`..=`30.0 %`).
#[must_use]
pub fn compute_potential_schedule_savings(
    avg_target: f64,
    comfort_temp: f64,
    eco_temp: f64,
    schedule_active: bool,
) -> f64 {
    if avg_target <= FROST_IDLE_MAX_TEMP {
        return 0.0;
    }
    let opt_comfort = comfort_temp.min(RECOMMENDED_COMFORT_TEMP);
    let opt_eco = eco_temp.min(RECOMMENDED_ECO_TEMP);
    let mut opt_daily =
        (RECOMMENDED_COMFORT_HOURS * opt_comfort + RECOMMENDED_ECO_HOURS * opt_eco) / 24.0;
    if !schedule_active {
        let user_daily =
            (RECOMMENDED_COMFORT_HOURS * comfort_temp + RECOMMENDED_ECO_HOURS * eco_temp) / 24.0;
        opt_daily = opt_daily.min(user_daily);
    }
    let savable_k = (avg_target - opt_daily).max(0.0);
    round_decimals(
        (savable_k * VDI_SAVINGS_PER_KELVIN).min(MAX_SAVINGS_PERCENT),
        1,
    )
}

// =============================================================================
/// Builds the 5-day [`HeatingForecast`] for a single thermostat.
#[must_use]
pub fn build_device_forecast(req: &DeviceForecastRequest<'_>) -> HeatingForecast {
    let days = normalize_forecast_days(req.daily_forecast, req.fallback_outdoor_temp, req.now_ts);
    let eff_intensity = if req.learned_intensity > 0.0 {
        req.learned_intensity
            .clamp(MIN_LEARNED_INTENSITY, MAX_LEARNED_INTENSITY)
    } else {
        IDEAL_DEMAND_INTENSITY
    };
    let (proj_min, avg_out, avg_tgt) = simulate_days(&days, req, eff_intensity);
    let sched_active = req.rules.get(HEATING_SCHEDULE).is_some_and(|r| r.is_active);
    let (trend_dir, trend_pct) = compute_heating_trend(proj_min, req.baseline_daily_minutes);
    let pot_sav =
        compute_potential_schedule_savings(avg_tgt, req.comfort_temp, req.eco_temp, sched_active);

    HeatingForecast {
        trend_direction: trend_dir.to_owned(),
        trend_delta_percent: trend_pct,
        projected_daily_heating_minutes: proj_min,
        potential_schedule_savings_percent: pot_sav,
        baseline_daily_heating_minutes: round_decimals(req.baseline_daily_minutes, 1),
        avg_forecast_outdoor_temp: avg_out,
        avg_scheduled_target_temp: avg_tgt,
        forecast_days: days.len(),
    }
}

// =============================================================================
/// Aggregates multiple per-thermostat [`HeatingForecast`] objects for a room group or house.
#[must_use]
pub fn aggregate_forecasts(forecasts: &[HeatingForecast]) -> Option<HeatingForecast> {
    if forecasts.is_empty() {
        return None;
    }
    let n = forecasts.len() as f64;
    let proj_min = round_decimals(
        forecasts
            .iter()
            .map(|f| f.projected_daily_heating_minutes)
            .sum::<f64>()
            / n,
        1,
    );
    let base_min = round_decimals(
        forecasts
            .iter()
            .map(|f| f.baseline_daily_heating_minutes)
            .sum::<f64>()
            / n,
        1,
    );
    let pot_sav = round_decimals(
        forecasts
            .iter()
            .map(|f| f.potential_schedule_savings_percent)
            .sum::<f64>()
            / n,
        1,
    );
    let avg_out = round_decimals(
        forecasts
            .iter()
            .map(|f| f.avg_forecast_outdoor_temp)
            .sum::<f64>()
            / n,
        1,
    );
    let avg_tgt = round_decimals(
        forecasts
            .iter()
            .map(|f| f.avg_scheduled_target_temp)
            .sum::<f64>()
            / n,
        1,
    );
    let (trend_dir, trend_pct) = compute_heating_trend(proj_min, base_min);

    Some(HeatingForecast {
        trend_direction: trend_dir.to_owned(),
        trend_delta_percent: trend_pct,
        projected_daily_heating_minutes: proj_min,
        potential_schedule_savings_percent: pot_sav,
        baseline_daily_heating_minutes: base_min,
        avg_forecast_outdoor_temp: avg_out,
        avg_scheduled_target_temp: avg_tgt,
        forecast_days: forecasts
            .first()
            .map_or(FORECAST_DAYS_DEFAULT, |f| f.forecast_days),
    })
}

// =============================================================================
/// Normalizes up to 5 daily weather forecast entries with deterministic offline fallback.
fn normalize_forecast_days(
    daily: &[ForecastDayInput],
    fallback_temp: f64,
    now_ts: Option<f64>,
) -> Vec<NormalizedDay> {
    let secs = now_ts.unwrap_or_else(now_epoch_f64).floor() as i64;
    let base_dt = Utc
        .timestamp_opt(secs, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .date_naive();
    if !daily.is_empty() {
        return daily
            .iter()
            .take(FORECAST_DAYS_DEFAULT)
            .enumerate()
            .map(|(idx, raw)| parse_single_day(Some(raw), idx, base_dt, fallback_temp))
            .collect();
    }
    (0..FORECAST_DAYS_DEFAULT)
        .map(|idx| parse_single_day(None, idx, base_dt, fallback_temp))
        .collect()
}

// =============================================================================
/// Converts one [`ForecastDayInput`] (or fallback index) into a [`NormalizedDay`].
fn parse_single_day(
    raw: Option<&ForecastDayInput>,
    index: usize,
    base_date: NaiveDate,
    fallback_temp: f64,
) -> NormalizedDay {
    let date = raw
        .and_then(|d| NaiveDate::parse_from_str(d.date.get(..10).unwrap_or(""), "%Y-%m-%d").ok())
        .unwrap_or_else(|| base_date + ChronoDuration::days(index as i64));
    let raw_min = raw.and_then(|d| d.temp_min).unwrap_or(fallback_temp);
    let raw_max = raw.and_then(|d| d.temp_max).unwrap_or(fallback_temp);
    let (min_f, max_f) = (raw_min.min(raw_max), raw_min.max(raw_max));
    let wd_idx = date.weekday().num_days_from_monday() as usize;
    let day_ts = date
        .and_hms_opt(0, 0, 0)
        .map_or(0.0, |dt| dt.and_utc().timestamp() as f64);

    NormalizedDay {
        weekday: WEEKDAYS[wd_idx.min(6)],
        month_day: format!("{:02}-{:02}", date.month(), date.day()),
        day_ts,
        temp_max: max_f,
        temp_mean: (min_f + max_f) / 2.0,
    }
}

// =============================================================================
/// Simulates daily HDD runtime minutes across all normalized forecast days.
fn simulate_days(
    days: &[NormalizedDay],
    req: &DeviceForecastRequest<'_>,
    eff_intensity: f64,
) -> (f64, f64, f64) {
    let n = (days.len().max(1)) as f64;
    let (mut sum_min, mut sum_out, mut sum_tgt) = (0.0, 0.0, 0.0);
    for day in days {
        let (tgt, limit_t, paused) = resolve_day_target(day, req);
        let hdd = if paused || day.temp_mean >= limit_t {
            0.0
        } else {
            (tgt - day.temp_mean).max(0.0)
        };
        sum_min += (hdd * eff_intensity).min(MINUTES_PER_DAY);
        sum_out += day.temp_mean;
        sum_tgt += tgt;
    }
    (
        round_decimals(sum_min / n, 1),
        round_decimals(sum_out / n, 1),
        round_decimals(sum_tgt / n, 1),
    )
}
