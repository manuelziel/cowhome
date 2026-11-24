//! 4-Factor Thermal Heating Efficiency Score (`VDI 2067/3807`) & 5-Day Forecast queries.
//!
//! Orchestrates `state_history` window reads, `WEATHER_01` outdoor temperature averaging,
//! 7–14d learned building signatures, and device/group/house aggregation via
//! [`super::efficiency_math`].

use std::collections::HashMap;

use rusqlite::params;
use serde_json::{Map, Value, json};

use super::Database;
pub use super::efficiency_math::compute_weather_compensated_heating_rate;
use super::efficiency_math::{
    IntegratedMetrics, attach_forecast_fields, build_efficiency_payload,
    integrate_thermostat_samples,
};
use super::entities_sync::deduplicate_ains_by_norm;
use super::history::resolve_efficiency_window;
use crate::core::domain_constants::{
    analytics::efficiency_defaults::{
        FALLBACK_STEP_SECS, FORECAST_BASELINE_DAYS, FORECAST_SIGNATURE_DAYS,
        MIN_BASELINE_HDD_KELVIN, MIN_DAYS_SPAN, MINUTES_PER_HOUR, SECONDS_PER_DAY,
        SECONDS_PER_MINUTE,
    },
    automation::automation_defaults::{DEFAULT_HEATING_RATE_MIN_PER_C, PREHEAT_REF_OUTDOOR_TEMP_C},
    hardware::{
        device_types,
        special_devices::{GROUP_ID_PREFIX, WEATHER_AIN},
    },
};
use crate::core::heating_forecast::{
    DeviceForecastRequest, ForecastDayInput, HeatingForecast, aggregate_forecasts,
    build_device_forecast,
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::normalize_ain;

// =============================================================================
/// Evaluation context passed to [`Database::get_heating_efficiency`] and house/group aggregators.
#[derive(Debug, Clone, Default)]
pub struct AnalyticsContext<'a> {
    /// Optional start UNIX timestamp (seconds or milliseconds).
    pub since: Option<i64>,
    /// Optional time-window token (`"24h"`, `"7d"`, `"30d"`, ...).
    pub range_str: Option<&'a str>,
    /// Optional 5-day Open-Meteo daily forecast entries.
    pub daily_forecast: &'a [ForecastDayInput],
    /// Optional map of normalized AIN -> live target temperature (`°C`).
    pub live_targets: Option<&'a HashMap<String, f64>>,
    /// Optional live outdoor temperature (`°C`) from `WeatherService`.
    pub live_outdoor_temp: Option<f64>,
}

// =============================================================================
/// Weather-compensated heating-rate telemetry for a device or group scope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeHeatingMetrics {
    /// Learned or fallback base heating rate (`min / °C`).
    pub heating_rate_min_per_c: f64,
    /// Weather-compensated effective heating rate (`min / °C`).
    pub effective_heating_rate_min_per_c: f64,
    /// Effective heating speed (`°C / h`).
    pub heating_rate_c_per_hour: f64,
    /// Current outdoor temperature (`°C`), if known.
    pub outdoor_temp_c: Option<f64>,
    /// Clamped outdoor thermal-inertia multiplier (`0.70..=1.60`).
    pub outdoor_inertia_factor: f64,
}

impl Database {
    /// Resolves current outdoor temperature (`°C`) from live input or latest `WEATHER_01` row.
    #[must_use]
    pub fn resolve_current_outdoor_temp(&self, live_outdoor_temp: Option<f64>) -> Option<f64> {
        if let Some(t) = live_outdoor_temp.filter(|v| v.is_finite()) {
            return Some(round_decimals(t, 1));
        }
        let (snap, _) = self.get_latest_state_snapshot(WEATHER_AIN).ok()??;
        snap.get("temperature")
            .and_then(Value::as_f64)
            .map(|t| round_decimals(t, 1))
    }

    /// Resolves base and weather-compensated heating rates (`min/°C` & `°C/h`) across `ains`.
    #[must_use]
    pub fn resolve_scope_heating_metrics(
        &self,
        ains: &[String],
        live_outdoor_temp: Option<f64>,
        since: Option<i64>,
        range_str: Option<&str>,
    ) -> ScopeHeatingMetrics {
        let resolved: Vec<(f64, f64, f64)> = ains
            .iter()
            .filter_map(|a| self.resolve_heating_rate_and_window(a, since, range_str))
            .filter(|&(r, _, _)| r > 0.0)
            .collect();
        let base_rate = if resolved.is_empty() {
            DEFAULT_HEATING_RATE_MIN_PER_C
        } else {
            let sum: f64 = resolved.iter().map(|r| r.0).sum();
            round_decimals(sum / (resolved.len() as f64), 1)
        };
        let cur_out = self.resolve_current_outdoor_temp(live_outdoor_temp);
        let mut ref_out = PREHEAT_REF_OUTDOOR_TEMP_C;
        if !resolved.is_empty() {
            let start_ts = resolved.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
            let end_ts = resolved.iter().map(|r| r.2).fold(0.0_f64, f64::max);
            if let Ok(w_ent) = self.resolve_entity(WEATHER_AIN)
                && let Ok(samples) =
                    self.fetch_bounded_history_rows(w_ent.entity_id, start_ts, end_ts)
                && samples
                    .first()
                    .is_some_and(|&(ts, _)| (end_ts - ts) >= FALLBACK_STEP_SECS)
            {
                ref_out = self.average_outdoor_from_samples(end_ts, &samples);
            }
        }
        let (eff_rate, c_per_h, f_out) =
            compute_weather_compensated_heating_rate(base_rate, cur_out, ref_out);
        ScopeHeatingMetrics {
            heating_rate_min_per_c: base_rate,
            effective_heating_rate_min_per_c: eff_rate,
            heating_rate_c_per_hour: c_per_h,
            outdoor_temp_c: cur_out,
            outdoor_inertia_factor: f_out,
        }
    }

    /// Computes the 4-factor Thermal Heating Efficiency + 5-day forecast for a device or group.
    #[must_use]
    pub fn get_heating_efficiency(&self, ain: &str, ctx: &AnalyticsContext<'_>) -> Option<Value> {
        let trimmed = ain.trim();
        if trimmed.starts_with(GROUP_ID_PREFIX) {
            let grp = self.get_software_group(trimmed).ok()??;
            let members: Vec<String> = grp
                .get("members")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            return self.aggregate_efficiency_for_ains(&members, ctx);
        }
        let entity = self.resolve_entity(trimmed).ok()?;
        if !matches!(
            entity.device_type.as_str(),
            device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
        ) {
            return None;
        }
        let (start_ts, end_ts) = resolve_efficiency_window(ctx.since, ctx.range_str);
        let samples = self
            .fetch_bounded_history_rows(entity.entity_id, start_ts, end_ts)
            .ok()?;
        let (comfort, eco) = self.get_thermostat_presets(trimmed);
        let acc = integrate_thermostat_samples(&samples, end_ts, comfort, eco)?;
        let avg_out = self.resolve_average_outdoor_temp(start_ts, end_ts);
        let metrics = self.resolve_scope_heating_metrics(
            std::slice::from_ref(&entity.ain),
            ctx.live_outdoor_temp,
            ctx.since,
            ctx.range_str,
        );
        let forecast = self.compute_device_forecast(
            &entity.ain,
            entity.entity_id,
            &samples,
            (start_ts, end_ts),
            (comfort, eco),
            &acc,
            avg_out,
            ctx,
        );
        Some(build_efficiency_payload(
            &acc,
            avg_out,
            comfort,
            metrics.heating_rate_min_per_c,
            metrics.effective_heating_rate_min_per_c,
            Some(&forecast),
        ))
    }

    /// Averages `HeatingEfficiency` and `HeatingForecast` payloads across `ains`.
    #[must_use]
    pub fn aggregate_efficiency_for_ains(
        &self,
        ains: &[String],
        ctx: &AnalyticsContext<'_>,
    ) -> Option<Value> {
        let items: Vec<Value> = ains
            .iter()
            .filter_map(|a| self.get_heating_efficiency(a, ctx))
            .collect();
        if items.is_empty() {
            return None;
        }
        let n = items.len() as f64;
        let avg_field = |key: &str, dec: i32| -> f64 {
            let sum: f64 = items
                .iter()
                .filter_map(|it| it.get(key).and_then(Value::as_f64))
                .sum();
            round_decimals(sum / n, dec)
        };
        let avg_factor = |key: &str| -> f64 {
            let sum: f64 = items
                .iter()
                .filter_map(|it| it.get("factors")?.get(key)?.as_f64())
                .sum();
            round_decimals(sum / n, 2)
        };
        let forecasts: Vec<HeatingForecast> = items
            .iter()
            .filter_map(|it| serde_json::from_value(it.get("heating_forecast")?.clone()).ok())
            .collect();
        let agg_fc = aggregate_forecasts(&forecasts);
        let score = avg_field("score", 2);
        let r_base = avg_field("heating_rate_min_per_c", 1);
        let r_eff = avg_field("effective_heating_rate_min_per_c", 1).max(0.1);
        let mut payload = json!({
            "score": score,
            "score_percent": (score * 100.0).round() as i64,
            "heating_demand_minutes": avg_field("heating_demand_minutes", 1),
            "daily_heating_minutes": avg_field("daily_heating_minutes", 1),
            "observed_days": avg_field("observed_days", 2),
            "avg_indoor_temp": avg_field("avg_indoor_temp", 1),
            "avg_target_temp": avg_field("avg_target_temp", 1),
            "avg_outdoor_temp": items[0].get("avg_outdoor_temp").and_then(Value::as_f64).unwrap_or(10.0),
            "estimated_savings_percent": avg_field("estimated_savings_percent", 1),
            "heating_rate_min_per_c": r_base,
            "effective_heating_rate_min_per_c": r_eff,
            "heating_rate_c_per_hour": round_decimals(MINUTES_PER_HOUR / r_eff, 1),
            "overshoot_minutes": avg_field("overshoot_minutes", 1),
            "window_open_minutes": avg_field("window_open_minutes", 1),
            "factors": {
                "demand_score": avg_factor("demand_score"),
                "setback_score": avg_factor("setback_score"),
                "recovery_score": avg_factor("recovery_score"),
                "precision_score": avg_factor("precision_score"),
            },
        });
        attach_forecast_fields(&mut payload, agg_fc.as_ref());
        Some(payload)
    }

    /// Computes the house-wide `HeatingEfficiency` across all registered thermostats.
    #[must_use]
    pub fn get_house_heating_efficiency(&self, ctx: &AnalyticsContext<'_>) -> Option<Value> {
        let raw_ains = self
            .with_conn(|conn| {
                let mut stmt =
                    conn.prepare("SELECT ain FROM entities WHERE device_type IN (?, ?)")?;
                let rows = stmt
                    .query_map(
                        params![device_types::THERMOSTAT, device_types::THERMOSTAT_GROUP],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .ok()?;
        let ains = deduplicate_ains_by_norm(raw_ains);
        (!ains.is_empty()).then(|| self.aggregate_efficiency_for_ains(&ains, ctx))?
    }

    /// Computes time-weighted average outdoor temperature (`°C`) over `[start_ts, end_ts]`.
    fn resolve_average_outdoor_temp(&self, start_ts: f64, end_ts: f64) -> f64 {
        if let Ok(ent) = self.resolve_entity(WEATHER_AIN)
            && let Ok(samples) = self.fetch_bounded_history_rows(ent.entity_id, start_ts, end_ts)
            && !samples.is_empty()
        {
            return self.average_outdoor_from_samples(end_ts, &samples);
        }
        self.resolve_current_outdoor_temp(None)
            .unwrap_or(PREHEAT_REF_OUTDOOR_TEMP_C)
    }

    /// Integrates pre-fetched `WEATHER_01` samples into a time-weighted mean (`°C`).
    fn average_outdoor_from_samples(
        &self,
        end_ts: f64,
        samples: &[(f64, Map<String, Value>)],
    ) -> f64 {
        let mut total_dt = 0.0;
        let mut weighted = 0.0;
        for (idx, (ts, state)) in samples.iter().enumerate() {
            let Some(temp) = state.get("temperature").and_then(Value::as_f64) else {
                continue;
            };
            let next_ts = samples.get(idx + 1).map_or(end_ts, |(n_ts, _)| *n_ts);
            let raw_dt = next_ts - *ts;
            let dt = if raw_dt > 0.0 || samples.len() > 1 {
                raw_dt
            } else {
                FALLBACK_STEP_SECS
            };
            if dt > 0.0 {
                total_dt += dt;
                weighted += temp * dt;
            }
        }
        if total_dt > 0.0 {
            round_decimals(weighted / total_dt, 1)
        } else {
            self.resolve_current_outdoor_temp(None)
                .unwrap_or(PREHEAT_REF_OUTDOOR_TEMP_C)
        }
    }

    /// Resolves 7–14d learned intensity + baseline and builds a 5-day [`HeatingForecast`].
    #[allow(clippy::too_many_arguments)]
    fn compute_device_forecast(
        &self,
        ain: &str,
        entity_id: i64,
        samples: &[(f64, Map<String, Value>)],
        window: (f64, f64),
        presets: (f64, f64),
        acc: &IntegratedMetrics,
        avg_out: f64,
        ctx: &AnalyticsContext<'_>,
    ) -> HeatingForecast {
        let (group_id, rules, global_vac) = self.resolve_scope_forecast_rules(ain);
        let norm_ain = normalize_ain(ain);
        let live_tgt = ctx.live_targets.and_then(|m| m.get(&norm_ain).copied());
        let curr_tgt = live_tgt
            .or_else(|| samples.last()?.1.get("temperature_target")?.as_f64())
            .unwrap_or_else(|| round_decimals(acc.w_tgt / acc.dt, 1));
        let (intensity, base_min) =
            self.resolve_forecast_baseline(entity_id, window, presets, acc, avg_out);

        build_device_forecast(&DeviceForecastRequest {
            ain,
            group_id: group_id.as_deref(),
            rules: &rules,
            global_vacation: global_vac.as_ref(),
            current_target: curr_tgt,
            comfort_temp: presets.0,
            eco_temp: presets.1,
            learned_intensity: intensity,
            baseline_daily_minutes: base_min,
            fallback_outdoor_temp: avg_out,
            daily_forecast: ctx.daily_forecast,
            now_ts: None,
        })
    }

    /// Resolves `(learned_intensity_min_per_k_d, baseline_daily_minutes)` over 7–14 days.
    fn resolve_forecast_baseline(
        &self,
        entity_id: i64,
        (start_ts, end_ts): (f64, f64),
        (comfort, eco): (f64, f64),
        acc: &IntegratedMetrics,
        avg_out: f64,
    ) -> (f64, f64) {
        let span_7d = FORECAST_BASELINE_DAYS * SECONDS_PER_DAY;
        let start_7d = end_ts - span_7d;
        let is_7d = ((end_ts - start_ts) - span_7d).abs() <= FALLBACK_STEP_SECS;
        let (acc_7d, used_fallback) = if is_7d {
            (*acc, true)
        } else {
            let s7 = self
                .fetch_bounded_history_rows(entity_id, start_7d, end_ts)
                .unwrap_or_default();
            integrate_thermostat_samples(&s7, end_ts, comfort, eco)
                .map_or((*acc, true), |a| (a, false))
        };
        let days = (acc_7d.dt / SECONDS_PER_DAY).max(MIN_DAYS_SPAN);
        let dem_daily = (acc_7d.dem / SECONDS_PER_MINUTE) / days;
        let out_7d = if used_fallback {
            avg_out
        } else {
            self.resolve_average_outdoor_temp(start_7d, end_ts)
        };
        let mut intensity =
            dem_daily / ((acc_7d.w_in / acc_7d.dt) - out_7d).max(MIN_BASELINE_HDD_KELVIN);
        if dem_daily <= 0.0 {
            let start_14d = end_ts - FORECAST_SIGNATURE_DAYS * SECONDS_PER_DAY;
            let s14 = self
                .fetch_bounded_history_rows(entity_id, start_14d, end_ts)
                .unwrap_or_default();
            if let Some(a14) =
                integrate_thermostat_samples(&s14, end_ts, comfort, eco).filter(|a| a.dem > 0.0)
            {
                let d14 =
                    (a14.dem / SECONDS_PER_MINUTE) / (a14.dt / SECONDS_PER_DAY).max(MIN_DAYS_SPAN);
                let out14 = self.resolve_average_outdoor_temp(start_14d, end_ts);
                intensity = d14 / ((a14.w_in / a14.dt) - out14).max(MIN_BASELINE_HDD_KELVIN);
            } else {
                intensity = 0.0;
            }
        }
        (intensity, dem_daily)
    }
}
