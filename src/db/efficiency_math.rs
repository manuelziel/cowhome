//! Pure step-before integration & 4-factor `VDI 2067/3807` scoring math for `efficiency.rs`.
//!
//! Separates numerical time-series integration, boiler dead-time accumulation, normalized
//! `demand`/`setback`/`recovery`/`precision` sub-scores, and JSON payload assembly from
//! SQLite query orchestration.

use serde_json::{json, Map, Value};

use super::ramp_analytics::has_active_water_circulation;
use crate::core::domain_constants::{
    analytics::efficiency_defaults::{
        BASE_SETBACK_SCORE, DEMAND_DELTA_THRESHOLD_C, DEMAND_INTENSITY_SPAN, FALLBACK_STEP_SECS,
        HEATING_RATE_SPAN, IDEAL_DEMAND_INTENSITY, IDEAL_HEATING_RATE, IDEAL_MAX_DROP_K,
        IDEAL_RECOVERY_MINUTES, IDEAL_SETBACK_DELTA_K, MAX_DROP_SPAN_K, MAX_SAVINGS_PERCENT,
        MINUTES_PER_HOUR, MIN_DAYS_SPAN, MIN_RECOVERY_SUBSCORE, MIN_TEMP_DIFF_OUTDOOR_K,
        OVERSHOOT_DELTA_THRESHOLD_C, OVERSHOOT_PENALTY_PER_MIN, RECOVERY_DROP_WEIGHT,
        RECOVERY_MINUTES_SPAN, RECOVERY_RATE_WEIGHT, SECONDS_PER_DAY, SECONDS_PER_MINUTE,
        SETBACK_SCORE_SPAN, TREND_STABLE, VDI_SAVINGS_PER_KELVIN, WEIGHT_DEMAND, WEIGHT_PRECISION,
        WEIGHT_RECOVERY, WEIGHT_SETBACK, WINDOW_PENALTY_PER_MIN,
    },
    automation::automation_defaults::{
        DEAD_TIME_THRESHOLD_MINUTES, FROST_PROTECTION_TEMP, MAX_HEATING_RATE_MIN_PER_C,
        MIN_HEATING_RATE_MIN_PER_C, OUTDOOR_TEMP_THRESHOLD, PREHEAT_MAX_OUTDOOR_FACTOR,
        PREHEAT_MIN_OUTDOOR_FACTOR, PREHEAT_OUTDOOR_INERTIA_PER_K,
    },
};
use crate::core::heating_forecast::HeatingForecast;
use crate::core::psychrometrics::round_decimals;

// =============================================================================
/// Step-before thermostat integration accumulator over a bounded window.
#[derive(Debug, Clone, Copy)]
pub(super) struct IntegratedMetrics {
    /// Total observed online seconds (`> 0`).
    pub dt: f64,
    /// Time-weighted indoor temperature sum (`°C * s`).
    pub w_in: f64,
    /// Time-weighted target temperature sum (`°C * s`).
    pub w_tgt: f64,
    /// Minimum observed indoor temperature (`°C`).
    pub min_in: f64,
    /// Active heating demand seconds (excluding boiler dead-time).
    pub dem: f64,
    /// Pending uncirculated demand seconds (`<= 25 min` flushed, `> 25 min` dropped).
    pub idle: f64,
    /// Open-window seconds while `target > 8.0 °C`.
    pub win: f64,
    /// Room temperature overshoot seconds.
    pub over: f64,
}

// =============================================================================
/// Computes `(effective_rate_min_per_c, speed_c_per_hour, outdoor_inertia_factor)`.
#[must_use]
pub fn compute_weather_compensated_heating_rate(
    base_rate_min_per_c: f64,
    current_outdoor_temp: Option<f64>,
    ref_outdoor_temp: f64,
) -> (f64, f64, f64) {
    let f_out = current_outdoor_temp.map_or(1.0, |t_out| {
        let delta_w = ref_outdoor_temp - t_out;
        (1.0 + PREHEAT_OUTDOOR_INERTIA_PER_K * delta_w)
            .clamp(PREHEAT_MIN_OUTDOOR_FACTOR, PREHEAT_MAX_OUTDOOR_FACTOR)
    });
    let r_eff = round_decimals(
        (base_rate_min_per_c * f_out).clamp(MIN_HEATING_RATE_MIN_PER_C, MAX_HEATING_RATE_MIN_PER_C),
        1,
    );
    let c_per_h = round_decimals(MINUTES_PER_HOUR / r_eff, 1);
    (r_eff, c_per_h, round_decimals(f_out, 2))
}

// =============================================================================
/// Integrates chronological thermostat samples using step-before & dead-time rules.
pub(super) fn integrate_thermostat_samples(
    samples: &[(f64, Map<String, Value>)],
    end_ts: f64,
    comfort_temp: f64,
    eco_temp: f64,
) -> Option<IntegratedMetrics> {
    let mut acc = IntegratedMetrics {
        dt: 0.0,
        w_in: 0.0,
        w_tgt: 0.0,
        min_in: 99.0,
        dem: 0.0,
        idle: 0.0,
        win: 0.0,
        over: 0.0,
    };
    let max_idle = DEAD_TIME_THRESHOLD_MINUTES * SECONDS_PER_MINUTE;
    let mut prev_in: Option<f64> = None;

    for (idx, (ts, state)) in samples.iter().enumerate() {
        let t_in = state
            .get("temperature_current")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if state.get("present") == Some(&Value::Bool(false)) || t_in <= 0.0 {
            continue;
        }
        let t_tgt = state
            .get("temperature_target")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let win_open = state
            .get("window_open")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (next_ts, next_state) = samples
            .get(idx + 1)
            .map_or((end_ts, state), |(nts, ns)| (*nts, ns));
        let dt = (next_ts - *ts).max(FALLBACK_STEP_SECS);
        acc.dt += dt;
        acc.w_in += t_in * dt;
        acc.w_tgt += t_tgt * dt;
        acc.min_in = acc.min_in.min(t_in);

        let has_dem = !win_open && (t_tgt - t_in) >= DEMAND_DELTA_THRESHOLD_C;
        accumulate_demand_step(&mut acc, state, next_state, dt, has_dem, max_idle);
        if win_open && t_tgt > FROST_PROTECTION_TEMP {
            acc.win += dt;
        }
        let cooldown = t_tgt < comfort_temp && prev_in.is_some_and(|p| t_in < p);
        if !win_open
            && !cooldown
            && t_tgt > eco_temp
            && (t_in - t_tgt) > OVERSHOOT_DELTA_THRESHOLD_C
        {
            acc.over += dt;
        }
        prev_in = Some(t_in);
    }
    if acc.idle > 0.0 && acc.idle <= max_idle {
        acc.dem += acc.idle;
    }
    (acc.dt > 0.0).then_some(acc)
}

// =============================================================================
/// Accumulates active demand seconds while discarding idle boiler dead-time (`> 25 min`).
fn accumulate_demand_step(
    acc: &mut IntegratedMetrics,
    state: &Map<String, Value>,
    next_state: &Map<String, Value>,
    dt: f64,
    has_demand: bool,
    max_idle: f64,
) {
    if !has_demand {
        if acc.idle <= max_idle {
            acc.dem += acc.idle;
        }
        acc.idle = 0.0;
        return;
    }
    if has_active_water_circulation(state, next_state) {
        if acc.idle <= max_idle {
            acc.dem += acc.idle;
        }
        acc.idle = 0.0;
        acc.dem += dt;
    } else {
        acc.idle += dt;
    }
}

// =============================================================================
/// Computes the 4 normalized sub-scores (`0.0..=1.0`) from integrated metrics.
fn compute_efficiency_factors(
    acc: &IntegratedMetrics,
    avg_out: f64,
    comfort_temp: f64,
    heating_rate: f64,
) -> (f64, f64, f64, f64) {
    let days = (acc.dt / SECONDS_PER_DAY).max(MIN_DAYS_SPAN);
    let avg_in = acc.w_in / acc.dt;
    let avg_tgt = acc.w_tgt / acc.dt;
    let dem_24h = (acc.dem / SECONDS_PER_MINUTE) / days;
    let win_24h = (acc.win / SECONDS_PER_MINUTE) / days;
    let over_24h = (acc.over / SECONDS_PER_MINUTE) / days;

    let intensity = dem_24h / (avg_in - avg_out).max(MIN_TEMP_DIFF_OUTDOOR_K);
    let s_dem = (1.0 - (intensity - IDEAL_DEMAND_INTENSITY).max(0.0) / DEMAND_INTENSITY_SPAN)
        .clamp(0.0, 1.0);
    let setback = (comfort_temp - avg_tgt).max(0.0);
    let s_set = if avg_out >= OUTDOOR_TEMP_THRESHOLD && dem_24h == 0.0 {
        1.0
    } else {
        (BASE_SETBACK_SCORE + SETBACK_SCORE_SPAN * (setback / IDEAL_SETBACK_DELTA_K))
            .clamp(0.0, 1.0)
    };
    let drop = (comfort_temp - acc.min_in).max(0.0);
    let s_rate = (1.0 - (heating_rate - IDEAL_HEATING_RATE).max(0.0) / HEATING_RATE_SPAN)
        .clamp(MIN_RECOVERY_SUBSCORE, 1.0);
    let drop_pen = ((drop - IDEAL_MAX_DROP_K).max(0.0) / MAX_DROP_SPAN_K)
        .max(((drop * heating_rate) - IDEAL_RECOVERY_MINUTES).max(0.0) / RECOVERY_MINUTES_SPAN);
    let s_drop = (1.0 - drop_pen).clamp(MIN_RECOVERY_SUBSCORE, 1.0);
    let s_rec = RECOVERY_RATE_WEIGHT * s_rate + RECOVERY_DROP_WEIGHT * s_drop;
    let s_prec = (1.0 - WINDOW_PENALTY_PER_MIN * win_24h - OVERSHOOT_PENALTY_PER_MIN * over_24h)
        .clamp(0.0, 1.0);

    (
        round_decimals(s_dem, 2),
        round_decimals(s_set, 2),
        round_decimals(s_rec, 2),
        round_decimals(s_prec, 2),
    )
}

// =============================================================================
/// Assembles the complete `HeatingEfficiency` JSON payload matching `openapi.yaml`.
pub(super) fn build_efficiency_payload(
    acc: &IntegratedMetrics,
    avg_out: f64,
    comfort_temp: f64,
    rate_base: f64,
    rate_eff: f64,
    forecast: Option<&HeatingForecast>,
) -> Value {
    let r_base = round_decimals(rate_base, 1);
    let r_eff = round_decimals(rate_eff, 1).max(0.1);
    let (s_dem, s_set, s_rec, s_prec) =
        compute_efficiency_factors(acc, avg_out, comfort_temp, r_base);
    let score = round_decimals(
        WEIGHT_DEMAND * s_dem
            + WEIGHT_SETBACK * s_set
            + WEIGHT_RECOVERY * s_rec
            + WEIGHT_PRECISION * s_prec,
        2,
    );
    let setback = (comfort_temp - (acc.w_tgt / acc.dt)).max(0.0);
    let savings = round_decimals(
        (setback * VDI_SAVINGS_PER_KELVIN).min(MAX_SAVINGS_PERCENT),
        1,
    );
    let obs_days = round_decimals((acc.dt / SECONDS_PER_DAY).max(MIN_DAYS_SPAN), 2);
    let dem_min = round_decimals(acc.dem / SECONDS_PER_MINUTE, 1);

    let mut payload = json!({
        "score": score,
        "score_percent": (score * 100.0).round() as i64,
        "heating_demand_minutes": dem_min,
        "daily_heating_minutes": round_decimals(dem_min / obs_days, 1),
        "observed_days": obs_days,
        "avg_indoor_temp": round_decimals(acc.w_in / acc.dt, 1),
        "avg_target_temp": round_decimals(acc.w_tgt / acc.dt, 1),
        "avg_outdoor_temp": round_decimals(avg_out, 1),
        "estimated_savings_percent": savings,
        "heating_rate_min_per_c": r_base,
        "effective_heating_rate_min_per_c": r_eff,
        "heating_rate_c_per_hour": round_decimals(MINUTES_PER_HOUR / r_eff, 1),
        "overshoot_minutes": round_decimals(acc.over / SECONDS_PER_MINUTE, 1),
        "window_open_minutes": round_decimals(acc.win / SECONDS_PER_MINUTE, 1),
        "factors": {
            "demand_score": s_dem,
            "setback_score": s_set,
            "recovery_score": s_rec,
            "precision_score": s_prec,
        },
    });
    attach_forecast_fields(&mut payload, forecast);
    payload
}

// =============================================================================
/// Attaches `heating_forecast` plus flattened outlook properties onto `payload`.
pub(super) fn attach_forecast_fields(payload: &mut Value, forecast: Option<&HeatingForecast>) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    if let Some(fc) = forecast {
        obj.insert(
            String::from("heating_forecast"),
            serde_json::to_value(fc).unwrap_or(Value::Null),
        );
        obj.insert(String::from("trend_direction"), json!(fc.trend_direction));
        obj.insert(
            String::from("trend_delta_percent"),
            json!(fc.trend_delta_percent),
        );
        obj.insert(
            String::from("projected_daily_heating_minutes"),
            json!(fc.projected_daily_heating_minutes),
        );
        obj.insert(
            String::from("potential_schedule_savings_percent"),
            json!(fc.potential_schedule_savings_percent),
        );
    } else {
        obj.insert(String::from("heating_forecast"), Value::Null);
        obj.insert(String::from("trend_direction"), json!(TREND_STABLE));
    }
}
