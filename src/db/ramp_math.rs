//! Pure thermal-ramp extraction, `DIN EN 15232` dead-time filter, & diagnostic tracing math.
//!
//! Separates pure ramp/plateau state-machine algorithms from SQLite query orchestration
//! in [`super::ramp_analytics`].

use serde_json::{json, Value};

use crate::core::domain_constants::{
    analytics::{
        efficiency_defaults::SECONDS_PER_MINUTE,
        history_defaults::{DEADBAND_TEMP_C, FLOAT_EPSILON},
    },
    automation::automation_defaults::{
        DEAD_TIME_THRESHOLD_MINUTES, HEATING_RAMP_MIN_DEMAND_DELTA, HEATING_RAMP_MIN_FINAL_GAIN,
        HEATING_RAMP_MIN_TEMP_GAIN, MAX_HEATING_RATE_MIN_PER_C, MIN_HEATING_RATE_MIN_PER_C,
        VALVE_WARM_DELTA_C,
    },
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::format_epoch_utc_iso8601;

// =============================================================================
/// Compact thermostat history sample extracted for ramp learning and dead-time tracing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RampSample {
    /// UNIX epoch timestamp in seconds.
    pub ts: f64,
    /// Room temperature (`temperature_current`, `°C`).
    pub temp: f64,
    /// Optional radiator valve temperature (`temperature_hardware`, `°C`).
    pub hw_temp: Option<f64>,
    /// Setpoint temperature (`temperature_target`, `°C`).
    pub target: f64,
    /// Open-window flag (`window_open`).
    pub window_open: bool,
}

// =============================================================================
/// Evaluates hot-water circulation from indoor and radiator-valve temperatures.
#[must_use]
pub(super) fn circulation_between_temps(
    prev_in: f64,
    prev_hw: Option<f64>,
    curr_in: f64,
    curr_hw: Option<f64>,
) -> bool {
    if let (Some(p_h), Some(c_h)) = (prev_hw, curr_hw) {
        if (c_h - p_h) > FLOAT_EPSILON {
            return true;
        }
    }
    if curr_hw.is_some_and(|c_h| (c_h - curr_in) >= VALVE_WARM_DELTA_C) {
        return true;
    }
    if prev_hw.is_some_and(|p_h| (p_h - prev_in) >= VALVE_WARM_DELTA_C) {
        return true;
    }
    (curr_in - prev_in) > FLOAT_EPSILON
}

// =============================================================================
/// Extracts valid heating-ramp rates (`min / °C`) across chronological [`RampSample`]s.
pub(super) fn extract_heating_ramp_rates(rows: &[RampSample]) -> Vec<f64> {
    let mut rates = Vec::new();
    let mut anchor: Option<RampSample> = None;
    let mut pending: Option<RampSample> = None;
    let mut last_temp: Option<f64> = None;

    for &row in rows {
        if row.ts < 0.0 || row.temp <= 0.0 || row.window_open {
            anchor = None;
            pending = None;
            last_temp = None;
            continue;
        }
        let reset = anchor.is_none_or(|a| {
            (row.temp - a.temp) < -FLOAT_EPSILON
                || last_temp.is_some_and(|lt| (row.temp - lt) < -FLOAT_EPSILON)
                || !has_demand(&a, a.temp, HEATING_RAMP_MIN_FINAL_GAIN)
        });
        let Some(cur_anchor) = anchor.filter(|_| !reset) else {
            anchor = Some(row);
            pending = None;
            last_temp = Some(row.temp);
            continue;
        };
        last_temp = Some(row.temp);
        if (row.temp - cur_anchor.temp).abs() <= FLOAT_EPSILON {
            let (next_a, next_p) = update_plateau_anchors(cur_anchor, pending, row);
            anchor = Some(next_a);
            pending = next_p;
            continue;
        }
        let (eff_a, _) = resolve_effective_anchor(cur_anchor, pending, row);
        pending = None;
        anchor = Some(evaluate_rising_step(eff_a, row, &mut rates));
    }
    rates
}

// =============================================================================
/// Traces chronological samples into `(accepted_ramps, skipped_intervals)` diagnostic arrays.
pub(super) fn trace_heating_ramps(rows: &[RampSample]) -> (Vec<Value>, Vec<Value>) {
    let mut accepted = Vec::new();
    let mut skipped = Vec::new();
    let mut anchor: Option<RampSample> = None;
    let mut pending: Option<RampSample> = None;
    let mut last_temp: Option<f64> = None;

    for &row in rows {
        if row.ts < 0.0 || row.temp <= 0.0 || row.window_open {
            anchor = None;
            pending = None;
            last_temp = None;
            continue;
        }
        let reset = anchor.is_none_or(|a| {
            (row.temp - a.temp) < -FLOAT_EPSILON
                || last_temp.is_some_and(|lt| (row.temp - lt) < -FLOAT_EPSILON)
                || !has_demand(&a, a.temp, HEATING_RAMP_MIN_FINAL_GAIN)
        });
        let Some(cur_anchor) = anchor.filter(|_| !reset) else {
            anchor = Some(row);
            pending = None;
            last_temp = Some(row.temp);
            continue;
        };
        last_temp = Some(row.temp);
        if (row.temp - cur_anchor.temp).abs() <= FLOAT_EPSILON {
            let (next_a, next_p) = update_plateau_anchors(cur_anchor, pending, row);
            anchor = Some(next_a);
            pending = next_p;
            continue;
        }
        let (eff_a, shifted) = resolve_effective_anchor(cur_anchor, pending, row);
        if shifted {
            let dt_skip = (eff_a.ts - cur_anchor.ts) / SECONDS_PER_MINUTE;
            skipped.push(build_ramp_entry(
                &cur_anchor,
                &eff_a,
                dt_skip,
                0.0,
                "plateau_dead_time_shift",
            ));
        }
        pending = None;
        anchor = Some(trace_rising_step(eff_a, row, &mut accepted, &mut skipped));
    }
    (accepted, skipped)
}

// =============================================================================
/// Checks if `row.target` exceeds `ref_temp` by at least `min_delta`.
fn has_demand(row: &RampSample, ref_temp: f64, min_delta: f64) -> bool {
    (row.target - ref_temp) >= (min_delta - FLOAT_EPSILON)
}

// =============================================================================
/// Detects boiler dead-time (`dt_min > 25 min` without meaningful radiator circulation).
fn is_boiler_dead_time(anchor: &RampSample, row: &RampSample, dt_min: f64) -> bool {
    if dt_min <= DEAD_TIME_THRESHOLD_MINUTES {
        return false;
    }
    let d_in = row.temp - anchor.temp;
    if d_in >= (HEATING_RAMP_MIN_FINAL_GAIN - FLOAT_EPSILON) {
        return false;
    }
    if !circulation_between_temps(anchor.temp, anchor.hw_temp, row.temp, row.hw_temp) {
        return true;
    }
    if d_in < (DEADBAND_TEMP_C - FLOAT_EPSILON) {
        return true;
    }
    if let (Some(p_hw), Some(c_hw)) = (anchor.hw_temp, row.hw_temp) {
        let cold = (c_hw - p_hw) <= FLOAT_EPSILON && (c_hw - row.temp) < VALVE_WARM_DELTA_C;
        if cold && (p_hw - anchor.temp) < VALVE_WARM_DELTA_C {
            return true;
        }
    }
    false
}

// =============================================================================
/// Maintains `anchor` and tracks a candidate post-dead-time `pending` anchor on plateaus.
fn update_plateau_anchors(
    anchor: RampSample,
    pending: Option<RampSample>,
    row: RampSample,
) -> (RampSample, Option<RampSample>) {
    if !has_demand(&row, row.temp, HEATING_RAMP_MIN_FINAL_GAIN) {
        return (row, None);
    }
    let dt_min = (row.ts - anchor.ts) / SECONDS_PER_MINUTE;
    if dt_min <= 0.0 || dt_min > MAX_HEATING_RATE_MIN_PER_C {
        return (row, None);
    }
    if circulation_between_temps(anchor.temp, anchor.hw_temp, row.temp, row.hw_temp) {
        return (anchor, pending);
    }
    if dt_min > DEAD_TIME_THRESHOLD_MINUTES {
        let Some(p) = pending else {
            return (anchor, Some(row));
        };
        let p_dt = (row.ts - p.ts) / SECONDS_PER_MINUTE;
        if p_dt > DEAD_TIME_THRESHOLD_MINUTES
            && !circulation_between_temps(p.temp, p.hw_temp, row.temp, row.hw_temp)
        {
            return (anchor, Some(row));
        }
    }
    (anchor, pending)
}

// =============================================================================
/// Selects `pending` over `anchor` when temperature rises after plateau dead-time.
fn resolve_effective_anchor(
    anchor: RampSample,
    pending: Option<RampSample>,
    row: RampSample,
) -> (RampSample, bool) {
    if let Some(p) = pending {
        let p_dt = (row.ts - p.ts) / SECONDS_PER_MINUTE;
        let p_dtemp = row.temp - p.temp;
        if p_dt > 0.0 && p_dtemp > FLOAT_EPSILON && (p_dt / p_dtemp) >= MIN_HEATING_RATE_MIN_PER_C {
            return (p, true);
        }
    }
    (anchor, false)
}

// =============================================================================
/// Evaluates a rising step and appends valid `min / °C` ramp rates into `rates`.
fn evaluate_rising_step(anchor: RampSample, row: RampSample, rates: &mut Vec<f64>) -> RampSample {
    let dt_min = (row.ts - anchor.ts) / SECONDS_PER_MINUTE;
    let d_temp = row.temp - anchor.temp;
    if dt_min <= 0.0
        || dt_min > MAX_HEATING_RATE_MIN_PER_C
        || is_boiler_dead_time(&anchor, &row, dt_min)
        || !has_demand(&row, anchor.temp, HEATING_RAMP_MIN_FINAL_GAIN)
    {
        return row;
    }
    let min_full = HEATING_RAMP_MIN_TEMP_GAIN - FLOAT_EPSILON;
    let min_final = HEATING_RAMP_MIN_FINAL_GAIN - FLOAT_EPSILON;
    let still_demanding = has_demand(&row, row.temp, HEATING_RAMP_MIN_DEMAND_DELTA);
    let reached_tgt = (row.target - row.temp) <= (DEADBAND_TEMP_C + FLOAT_EPSILON);
    if d_temp >= min_full || (!still_demanding && d_temp >= min_final) {
        if has_demand(&anchor, anchor.temp, HEATING_RAMP_MIN_FINAL_GAIN) {
            let rate = dt_min / d_temp;
            if (MIN_HEATING_RATE_MIN_PER_C..=MAX_HEATING_RATE_MIN_PER_C).contains(&rate) {
                rates.push(rate);
            }
        }
        return row;
    }
    if reached_tgt {
        row
    } else {
        anchor
    }
}

// =============================================================================
/// Traces a single upward temperature step and records diagnostic reasons.
fn trace_rising_step(
    anchor: RampSample,
    row: RampSample,
    accepted: &mut Vec<Value>,
    skipped: &mut Vec<Value>,
) -> RampSample {
    let dt_min = (row.ts - anchor.ts) / SECONDS_PER_MINUTE;
    let d_temp = row.temp - anchor.temp;
    if dt_min <= 0.0 || dt_min > MAX_HEATING_RATE_MIN_PER_C {
        skipped.push(build_ramp_entry(
            &anchor,
            &row,
            dt_min,
            d_temp,
            "exceeds_max_duration",
        ));
        return row;
    }
    if is_boiler_dead_time(&anchor, &row, dt_min) {
        skipped.push(build_ramp_entry(
            &anchor,
            &row,
            dt_min,
            d_temp,
            "boiler_dead_time",
        ));
        return row;
    }
    if !has_demand(&row, anchor.temp, HEATING_RAMP_MIN_FINAL_GAIN) {
        return row;
    }
    let mut rates = Vec::new();
    let next = evaluate_rising_step(anchor, row, &mut rates);
    if !rates.is_empty() {
        accepted.push(build_ramp_entry(&anchor, &row, dt_min, d_temp, "accepted"));
    } else if (next.ts - row.ts).abs() <= FLOAT_EPSILON
        && d_temp >= (HEATING_RAMP_MIN_FINAL_GAIN - FLOAT_EPSILON)
    {
        skipped.push(build_ramp_entry(
            &anchor,
            &row,
            dt_min,
            d_temp,
            "rate_out_of_bounds",
        ));
    }
    next
}

// =============================================================================
/// Builds a structured JSON diagnostic object for a ramp or skipped interval.
fn build_ramp_entry(
    anchor: &RampSample,
    row: &RampSample,
    dt_min: f64,
    d_temp: f64,
    reason: &str,
) -> Value {
    let start_ts = anchor.ts.max(0.0) as u64;
    let end_ts = row.ts.max(0.0) as u64;
    let rate = (d_temp > FLOAT_EPSILON).then(|| round_decimals(dt_min / d_temp, 1));
    let hw_delta = anchor
        .hw_temp
        .zip(row.hw_temp)
        .map(|(a, r)| round_decimals(r - a, 2));
    json!({
        "start_ts": start_ts,
        "end_ts": end_ts,
        "start_time": format_epoch_utc_iso8601(start_ts),
        "end_time": format_epoch_utc_iso8601(end_ts),
        "dt_min": round_decimals(dt_min, 1),
        "temp_start": round_decimals(anchor.temp, 2),
        "temp_end": round_decimals(row.temp, 2),
        "d_temp": round_decimals(d_temp, 2),
        "hw_temp_delta": hw_delta,
        "rate_min_per_c": rate,
        "reason": reason,
    })
}
