//! Smart Energy metering (`DIN EN IEC 62053-21`) and tariff cost forecasting (`EnWG § 40`).
//!
//! Performs monotonic `TOTAL_INCREASING` cumulative Wh integration with transient `0.0 Wh`
//! dropout filtering and counter-reset tolerance, local-midnight day slicing (`chrono-tz`),
//! and house-only monthly base-fee proration (`device == "house"`).

use serde::{Deserialize, Serialize};

use crate::core::domain_constants::analytics::{
    efficiency_defaults::{
        SECONDS_PER_DAY, TREND_DEADBAND_PERCENT, TREND_FALLING, TREND_MAX_PERCENT,
        TREND_MIN_PERCENT, TREND_RISING, TREND_STABLE,
    },
    energy_defaults::{
        COUNTER_RESET_TOLERANCE_WH, CURRENCY, IDENTIFIER_HOUSE, MIN_ACTIVE_DAILY_KWH,
        MIN_BASELINE_DAILY_KWH, MIN_OBSERVED_DAYS, MONTH_DAYS, MONTHS_PER_YEAR,
        TREND_DEADBAND_KWH_DAILY, WEEK_DAYS, WH_PER_KWH, YEAR_DAYS,
    },
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::{now_epoch_f64, resolve_local_midnight_ts};

// =============================================================================
/// Chronological state sample extracted from `state_history` for a smart plug or meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergySample {
    /// Sample UNIX epoch timestamp in seconds.
    pub timestamp: f64,
    /// Whether the device was online (`present != false`).
    pub present: bool,
    /// Instantaneous active power (`W`), negative during solar feed-in.
    pub power_current_w: Option<f64>,
    /// Monotonic cumulative energy counter (`Wh`).
    pub energy_total_wh: Option<f64>,
}

// =============================================================================
/// 30-day and 365-day energy consumption and cost projection block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyForecastBlock {
    /// Time-normalized daily average consumption (`kWh/day`).
    pub daily_avg_kwh: f64,
    /// Projected 30-day consumption (`kWh`).
    pub projected_month_kwh: f64,
    /// Projected 30-day cost (`EUR`).
    pub projected_month_cost_eur: f64,
    /// Projected 365-day consumption (`kWh`).
    pub projected_year_kwh: f64,
    /// Projected 365-day cost (`EUR`).
    pub projected_year_cost_eur: f64,
    /// Consumption trend direction (`"rising"`, `"falling"`, or `"stable"`).
    pub trend_direction: String,
    /// Clamped 7d-vs-30d percentage delta (`-100.0`..=`200.0`).
    pub trend_delta_percent: f64,
}

// =============================================================================
/// Complete Smart Energy summary returned on `/api/v1/state`, `/history`, and `/system/energy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergySummary {
    /// Device AIN, software group ID, or `"house"`.
    pub device: String,
    /// Number of aggregated meters/plugs (present only on group/house totals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_count: Option<usize>,
    /// Active working electricity tariff (`EUR/kWh`).
    pub electricity_price_eur_per_kwh: f64,
    /// Configured monthly base fee (`EUR/month`).
    pub electricity_base_fee_eur_per_month: f64,
    /// ISO 4217 currency code (`"EUR"`).
    pub currency: String,
    /// Latest instantaneous power (`W`).
    pub power_current_w: f64,
    /// Latest cumulative counter reading (`Wh`).
    pub energy_total_wh: f64,
    /// Latest cumulative counter reading (`kWh`).
    pub energy_total_kwh: f64,
    /// Energy consumed since `00:00` local home time (`kWh`).
    pub today_kwh: f64,
    /// Cost incurred today (`EUR`, includes `+base_fee/30` when `device == "house"`).
    pub today_cost_eur: f64,
    /// Rolling 7-day energy consumption (`kWh`).
    pub week_kwh: f64,
    /// Rolling 7-day cost (`EUR`).
    pub week_cost_eur: f64,
    /// Rolling 30-day energy consumption (`kWh`).
    pub month_kwh: f64,
    /// Rolling 30-day cost (`EUR`).
    pub month_cost_eur: f64,
    /// Rolling 365-day energy consumption (`kWh`).
    pub year_kwh: f64,
    /// Rolling 365-day cost (`EUR`).
    pub year_cost_eur: f64,
    /// Flattened projection fields matching OpenAPI v1 contract.
    #[serde(flatten)]
    pub flat_forecast: EnergyForecastBlock,
    /// Nested `"forecast"` mirror object matching OpenAPI v1 contract.
    pub forecast: EnergyForecastBlock,
}

// =============================================================================
/// Slices chronological `samples` to `[start_ts, end_ts]` with a pre-window baseline anchor.
#[must_use]
pub fn slice_bounded_samples(
    samples: &[EnergySample],
    start_ts: f64,
    end_ts: f64,
) -> Vec<EnergySample> {
    let mut baseline: Option<EnergySample> = None;
    let mut window_rows = Vec::new();
    for &sample in samples {
        if sample.timestamp < start_ts {
            if sample.energy_total_wh.is_some() {
                baseline = Some(EnergySample {
                    timestamp: start_ts,
                    ..sample
                });
            }
        } else if sample.timestamp <= end_ts {
            window_rows.push(sample);
        }
    }
    if let Some(anchor) = baseline {
        let mut out = Vec::with_capacity(window_rows.len() + 1);
        out.push(anchor);
        out.extend(window_rows);
        out
    } else {
        window_rows
    }
}

// =============================================================================
/// Integrates cumulative `Wh` deltas (`TOTAL_INCREASING`) with dropout and reset protection.
///
/// Returns `(accumulated_delta_wh, observed_span_seconds)`.
#[must_use]
pub fn integrate_counter_window(samples: &[EnergySample], end_ts: f64) -> (f64, f64) {
    let points = extract_valid_wh_points(samples);
    let Some(&(first_ts, first_wh)) = points.first() else {
        return (0.0, 0.0);
    };
    let obs_secs = (end_ts - first_ts).max(0.0);
    if points.len() < 2 {
        return (0.0, obs_secs);
    }
    let mut delta_wh = 0.0;
    let mut prev_wh = first_wh;
    for idx in 1..points.len() {
        let curr_wh = points[idx].1;
        if curr_wh == 0.0 && prev_wh > 0.0 {
            if !is_transient_zero_dropout(&points, idx, prev_wh) {
                prev_wh = 0.0;
            }
            continue;
        }
        if curr_wh >= prev_wh {
            delta_wh += curr_wh - prev_wh;
            prev_wh = curr_wh;
        } else if (prev_wh - curr_wh) > COUNTER_RESET_TOLERANCE_WH {
            delta_wh += curr_wh;
            prev_wh = curr_wh;
        }
    }
    (delta_wh.max(0.0), obs_secs)
}

// =============================================================================
/// Filters online samples containing non-negative finite `energy_total_wh` values.
fn extract_valid_wh_points(samples: &[EnergySample]) -> Vec<(f64, f64)> {
    samples
        .iter()
        .filter(|s| s.present)
        .filter_map(|s| {
            s.energy_total_wh
                .filter(|&wh| wh.is_finite() && wh >= 0.0)
                .map(|wh| (s.timestamp, wh))
        })
        .collect()
}

// =============================================================================
/// Determines whether a `0.0 Wh` reading after `prev_wh > 0` is a temporary telemetry dropout.
fn is_transient_zero_dropout(points: &[(f64, f64)], idx: usize, prev_wh: f64) -> bool {
    for &(_, future_wh) in &points[idx + 1..] {
        if future_wh > 0.0 {
            return future_wh >= (prev_wh - COUNTER_RESET_TOLERANCE_WH);
        }
    }
    true
}

// =============================================================================
/// Computes consumption trend direction and clamped percentage delta (`rate_7d` vs `rate_30d`).
#[must_use]
pub fn compute_energy_trend(rate_7d: f64, rate_30d: f64) -> (&'static str, f64) {
    if rate_7d < MIN_ACTIVE_DAILY_KWH && rate_30d < MIN_ACTIVE_DAILY_KWH {
        return (TREND_STABLE, 0.0);
    }
    let abs_diff = rate_7d - rate_30d;
    let denom = rate_30d.max(MIN_BASELINE_DAILY_KWH);
    let raw_pct = (abs_diff / denom) * 100.0;
    let clamped_pct = round_decimals(raw_pct.clamp(TREND_MIN_PERCENT, TREND_MAX_PERCENT), 1);
    if clamped_pct.abs() < TREND_DEADBAND_PERCENT || abs_diff.abs() < TREND_DEADBAND_KWH_DAILY {
        return (TREND_STABLE, clamped_pct);
    }
    let dir = if clamped_pct > 0.0 {
        TREND_RISING
    } else {
        TREND_FALLING
    };
    (dir, clamped_pct)
}

// =============================================================================
/// Builds a complete [`EnergySummary`] for a single smart switch or optical grid meter.
#[must_use]
pub fn build_device_energy_summary(
    ain: &str,
    samples_365d: &[EnergySample],
    price_eur_per_kwh: f64,
    base_fee_eur_per_month: f64,
    now_ts: Option<f64>,
    tz_name: Option<&str>,
) -> EnergySummary {
    let eff_now = now_ts.unwrap_or_else(now_epoch_f64);
    let price = round_decimals(price_eur_per_kwh.max(0.0), 4);
    let base_fee = round_decimals(base_fee_eur_per_month.max(0.0), 2);
    let midnight_ts = resolve_local_midnight_ts(eff_now, tz_name);
    let (kwh_quad, rate_7d, rate_30d) = compute_window_rates(samples_365d, eff_now, midnight_ts);
    let (power_w, total_wh) = extract_latest_telemetry(samples_365d);
    let (trend_dir, trend_pct) = compute_energy_trend(rate_7d, rate_30d);
    let eff_base = if ain == IDENTIFIER_HOUSE {
        base_fee
    } else {
        0.0
    };
    let forecast = build_forecast_block(rate_30d, price, trend_dir, trend_pct, eff_base);
    assemble_summary(
        ain, None, price, base_fee, power_w, total_wh, kwh_quad, forecast,
    )
}

// =============================================================================
/// Aggregates multiple device [`EnergySummary`] payloads into a group or `"house"` total.
#[must_use]
pub fn aggregate_energy_summaries(
    summaries: &[EnergySummary],
    price_eur_per_kwh: f64,
    base_fee_eur_per_month: f64,
    identifier: &str,
) -> Option<EnergySummary> {
    if summaries.is_empty() {
        return None;
    }
    let price = round_decimals(price_eur_per_kwh.max(0.0), 4);
    let base_fee = round_decimals(base_fee_eur_per_month.max(0.0), 2);
    let eff_base = if identifier == IDENTIFIER_HOUSE {
        base_fee
    } else {
        0.0
    };
    let kwh_quad = (
        summaries.iter().map(|s| s.today_kwh).sum(),
        summaries.iter().map(|s| s.week_kwh).sum(),
        summaries.iter().map(|s| s.month_kwh).sum(),
        summaries.iter().map(|s| s.year_kwh).sum(),
    );
    let daily_rate: f64 = summaries
        .iter()
        .map(|s| s.flat_forecast.daily_avg_kwh)
        .sum();
    let rate_7d: f64 = summaries.iter().map(reconstruct_rate_7d).sum();
    let (trend_dir, trend_pct) = compute_energy_trend(rate_7d, daily_rate);
    let forecast = build_forecast_block(daily_rate, price, trend_dir, trend_pct, eff_base);
    let power_w = round_decimals(summaries.iter().map(|s| s.power_current_w).sum(), 1);
    let total_wh = round_decimals(summaries.iter().map(|s| s.energy_total_wh).sum(), 1);

    Some(assemble_summary(
        identifier,
        Some(summaries.len()),
        price,
        base_fee,
        power_w,
        total_wh,
        kwh_quad,
        forecast,
    ))
}

// =============================================================================
/// Integrates today, 7d, 30d, and 365d windows and returns `((today, week, month, year), r7, r30)`.
fn compute_window_rates(
    samples_365d: &[EnergySample],
    eff_now: f64,
    midnight_ts: f64,
) -> ((f64, f64, f64, f64), f64, f64) {
    let s_today = slice_bounded_samples(samples_365d, midnight_ts, eff_now);
    let s_7d = slice_bounded_samples(samples_365d, eff_now - WEEK_DAYS * SECONDS_PER_DAY, eff_now);
    let s_30d = slice_bounded_samples(
        samples_365d,
        eff_now - MONTH_DAYS * SECONDS_PER_DAY,
        eff_now,
    );

    let (wh_today, _) = integrate_counter_window(&s_today, eff_now);
    let (wh_7d, obs_7d) = integrate_counter_window(&s_7d, eff_now);
    let (wh_30d, obs_30d) = integrate_counter_window(&s_30d, eff_now);
    let (wh_365d, _) = integrate_counter_window(samples_365d, eff_now);

    let days_7d = if obs_7d > 0.0 {
        (obs_7d / SECONDS_PER_DAY).max(MIN_OBSERVED_DAYS)
    } else {
        0.0
    };
    let days_30d = if obs_30d > 0.0 {
        (obs_30d / SECONDS_PER_DAY).max(MIN_OBSERVED_DAYS)
    } else {
        0.0
    };
    let rate_7d = if days_7d > 0.0 {
        (wh_7d / WH_PER_KWH) / days_7d
    } else {
        0.0
    };
    let rate_30d = if days_30d > 0.0 {
        (wh_30d / WH_PER_KWH) / days_30d
    } else {
        rate_7d
    };
    (
        (
            wh_today / WH_PER_KWH,
            wh_7d / WH_PER_KWH,
            wh_30d / WH_PER_KWH,
            wh_365d / WH_PER_KWH,
        ),
        rate_7d,
        rate_30d,
    )
}

// =============================================================================
/// Extracts latest `power_current_w` and latest positive `energy_total_wh` from `samples`.
fn extract_latest_telemetry(samples: &[EnergySample]) -> (f64, f64) {
    let power_w = samples
        .last()
        .and_then(|s| s.power_current_w)
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);
    let total_wh = extract_valid_wh_points(samples)
        .into_iter()
        .rev()
        .map(|(_, wh)| wh)
        .find(|&wh| wh > 0.0)
        .unwrap_or(0.0);
    (round_decimals(power_w, 1), round_decimals(total_wh, 1))
}

// =============================================================================
/// Builds an [`EnergyForecastBlock`] for 30-day and 365-day consumption and cost horizons.
fn build_forecast_block(
    daily_rate: f64,
    price: f64,
    trend_dir: &str,
    trend_pct: f64,
    base_fee_applied: f64,
) -> EnergyForecastBlock {
    let proj_month_kwh = round_decimals(daily_rate * MONTH_DAYS, 3);
    let proj_year_kwh = round_decimals(daily_rate * YEAR_DAYS, 3);
    EnergyForecastBlock {
        daily_avg_kwh: round_decimals(daily_rate, 3),
        projected_month_kwh: proj_month_kwh,
        projected_month_cost_eur: round_decimals(proj_month_kwh * price + base_fee_applied, 2),
        projected_year_kwh: proj_year_kwh,
        projected_year_cost_eur: round_decimals(
            proj_year_kwh * price + base_fee_applied * MONTHS_PER_YEAR,
            2,
        ),
        trend_direction: trend_dir.to_owned(),
        trend_delta_percent: trend_pct,
    }
}

// =============================================================================
/// Reconstructs the 7-day normalized rate (`kWh/d`) from a device [`EnergySummary`].
fn reconstruct_rate_7d(summary: &EnergySummary) -> f64 {
    let r_30d = summary.flat_forecast.daily_avg_kwh.max(0.0);
    let pct = summary.flat_forecast.trend_delta_percent;
    let base = r_30d.max(MIN_BASELINE_DAILY_KWH);
    (r_30d + base * (pct / 100.0)).max(0.0)
}

// =============================================================================
/// Assembles the final [`EnergySummary`] with causal `EnWG § 40` house-only base-fee proration.
#[allow(clippy::too_many_arguments)]
fn assemble_summary(
    device: &str,
    device_count: Option<usize>,
    price: f64,
    base_fee: f64,
    power_w: f64,
    total_wh: f64,
    kwh_quad: (f64, f64, f64, f64),
    forecast: EnergyForecastBlock,
) -> EnergySummary {
    let (t_kwh, w_kwh, m_kwh, y_kwh) = (
        round_decimals(kwh_quad.0, 3),
        round_decimals(kwh_quad.1, 3),
        round_decimals(kwh_quad.2, 3),
        round_decimals(kwh_quad.3, 3),
    );
    let eff_base = if device == IDENTIFIER_HOUSE {
        base_fee
    } else {
        0.0
    };
    EnergySummary {
        device: device.to_owned(),
        device_count,
        electricity_price_eur_per_kwh: price,
        electricity_base_fee_eur_per_month: round_decimals(base_fee, 2),
        currency: CURRENCY.to_owned(),
        power_current_w: power_w,
        energy_total_wh: total_wh,
        energy_total_kwh: round_decimals(total_wh / WH_PER_KWH, 3),
        today_kwh: t_kwh,
        today_cost_eur: round_decimals(t_kwh * price + (eff_base / MONTH_DAYS), 2),
        week_kwh: w_kwh,
        week_cost_eur: round_decimals(w_kwh * price + (eff_base * WEEK_DAYS / MONTH_DAYS), 2),
        month_kwh: m_kwh,
        month_cost_eur: round_decimals(m_kwh * price + eff_base, 2),
        year_kwh: y_kwh,
        year_cost_eur: round_decimals(y_kwh * price + (eff_base * MONTHS_PER_YEAR), 2),
        flat_forecast: forecast.clone(),
        forecast,
    }
}
