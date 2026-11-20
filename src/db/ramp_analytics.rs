//! Thermal heating-ramp queries, temperature deltas, & AI ramp diagnostics on [`Database`].
//!
//! Orchestrates `state_history` window reads and delegates boiler dead-time filtering and
//! plateau-anchor extraction to [`super::ramp_math`].

use rusqlite::params;
use serde_json::{json, Map, Value};

use super::history::resolve_efficiency_window;
use super::ramp_math::{
    circulation_between_temps, extract_heating_ramp_rates, trace_heating_ramps, RampSample,
};
use super::Database;
use crate::core::domain_constants::automation::automation_defaults::{
    HEATING_RAMP_HISTORY_LIMIT, HEATING_RAMP_TRIM_MIN_SAMPLES, MAX_DIAGNOSTIC_RAMPS,
};
use crate::core::error::AppResult;
use crate::core::psychrometrics::round_decimals;

impl Database {
    /// Computes the indoor temperature delta (`°C`) for `ain` over the last `period_minutes`.
    #[must_use]
    pub fn get_temperature_delta(&self, ain: &str, period_minutes: u32) -> Option<f64> {
        let entity = self.resolve_entity(ain).ok()?;
        let modifier = format!("-{period_minutes} minutes");
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT json_extract(state_json, '$.temperature_current')
                 FROM state_history WHERE entity_id = ? AND timestamp >= datetime('now', ?)
                 ORDER BY timestamp ASC, state_id ASC",
            )?;
            let mut temps: Vec<f64> = stmt
                .query_map(params![entity.entity_id, modifier], |r| {
                    r.get::<_, Option<f64>>(0)
                })?
                .filter_map(|res| res.ok().flatten())
                .collect();
            if temps.len() == 1 {
                let baseline: Option<f64> = conn
                    .query_row(
                        "SELECT json_extract(state_json, '$.temperature_current')
                         FROM state_history WHERE entity_id = ? AND timestamp < datetime('now', ?)
                         ORDER BY timestamp DESC, state_id DESC LIMIT 1",
                        params![entity.entity_id, modifier],
                        |r| r.get::<_, Option<f64>>(0),
                    )
                    .ok()
                    .flatten();
                if let Some(base) = baseline {
                    temps.insert(0, base);
                }
            }
            Ok((temps.len() >= 2).then(|| {
                let first = temps.first().copied().unwrap_or(0.0);
                let last = temps.last().copied().unwrap_or(0.0);
                round_decimals(last - first, 2)
            }))
        })
        .ok()
        .flatten()
    }

    /// Resolves `(heating_rate_min_per_c, window_start_ts, window_end_ts)` for `ain`.
    #[must_use]
    pub(super) fn resolve_heating_rate_and_window(
        &self,
        ain: &str,
        since: Option<i64>,
        range_str: Option<&str>,
    ) -> Option<(f64, f64, f64)> {
        let entity = self.resolve_entity(ain).ok()?;
        let mut rows = self
            .fetch_ramp_samples(entity.entity_id, since, range_str)
            .ok()?;
        let mut rates = extract_heating_ramp_rates(&rows);
        if rates.is_empty() && (since.is_some() || range_str.is_some()) {
            rows = self.fetch_ramp_samples(entity.entity_id, None, None).ok()?;
            rates = extract_heating_ramp_rates(&rows);
        }
        if rates.is_empty() {
            return None;
        }
        let (accepted, _) = trace_heating_ramps(&rows);
        rates.sort_by(f64::total_cmp);
        let slice = if rates.len() >= HEATING_RAMP_TRIM_MIN_SAMPLES {
            &rates[1..rates.len() - 1]
        } else {
            &rates[..]
        };
        let avg = round_decimals(slice.iter().sum::<f64>() / (slice.len() as f64), 1);
        let (win_start, win_end) =
            if let (Some(first), Some(last)) = (accepted.first(), accepted.last()) {
                (
                    first.get("start_ts").and_then(Value::as_f64).unwrap_or(0.0),
                    last.get("end_ts").and_then(Value::as_f64).unwrap_or(0.0),
                )
            } else {
                let min_ts = rows.iter().map(|r| r.ts).fold(f64::INFINITY, f64::min);
                let max_ts = rows.iter().map(|r| r.ts).fold(0.0_f64, f64::max);
                (min_ts, max_ts)
            };
        Some((avg, win_start, win_end))
    }

    /// Calculates the outlier-trimmed average heating rate (`min / °C`) for `ain`.
    #[must_use]
    pub fn get_average_heating_rate(
        &self,
        ain: &str,
        since: Option<i64>,
        range_str: Option<&str>,
    ) -> Option<f64> {
        self.resolve_heating_rate_and_window(ain, since, range_str)
            .map(|(rate, _, _)| rate)
    }

    /// Generates structured heating-ramp and boiler dead-time diagnostics for `ain`.
    #[must_use]
    pub fn get_heating_ramp_diagnostics(
        &self,
        ain: &str,
        since: Option<i64>,
        range_str: Option<&str>,
    ) -> Value {
        let Ok(entity) = self.resolve_entity(ain) else {
            return json!({
                "device": ain,
                "average_rate_min_per_c": Value::Null,
                "accepted_ramps": [],
                "skipped_intervals": [],
            });
        };
        let rows = self
            .fetch_ramp_samples(entity.entity_id, since, range_str)
            .unwrap_or_default();
        let (mut accepted, mut skipped) = trace_heating_ramps(&rows);
        let mut fallback_used = false;
        if accepted.is_empty() && (since.is_some() || range_str.is_some()) {
            let fb_rows = self
                .fetch_ramp_samples(entity.entity_id, None, None)
                .unwrap_or_default();
            let traced = trace_heating_ramps(&fb_rows);
            accepted = traced.0;
            skipped = traced.1;
            fallback_used = true;
        }
        let avg_rate = self.get_average_heating_rate(ain, since, range_str);
        let acc_cnt = accepted.len();
        let skip_cnt = skipped.len();

        json!({
            "device": ain,
            "fallback_to_recent_200": fallback_used,
            "average_rate_min_per_c": avg_rate,
            "accepted_count": acc_cnt,
            "skipped_count": skip_cnt,
            "accepted_ramps": tail_slice(&accepted, MAX_DIAGNOSTIC_RAMPS),
            "skipped_intervals": tail_slice(&skipped, MAX_DIAGNOSTIC_RAMPS),
        })
    }

    /// Fetches chronological [`RampSample`] rows for `entity_id` (bounded window or last 200).
    fn fetch_ramp_samples(
        &self,
        entity_id: i64,
        since: Option<i64>,
        range_str: Option<&str>,
    ) -> AppResult<Vec<RampSample>> {
        self.with_conn(|conn| {
            if since.is_some() || range_str.is_some() {
                let (start_ts, end_ts) = resolve_efficiency_window(since, range_str);
                let mut stmt = conn.prepare(
                    "SELECT CAST(strftime('%s', timestamp) AS REAL),
                            json_extract(state_json, '$.temperature_current'),
                            json_extract(state_json, '$.temperature_hardware'),
                            json_extract(state_json, '$.temperature_target'),
                            json_extract(state_json, '$.window_open')
                     FROM state_history WHERE entity_id = ?
                       AND timestamp >= datetime(?, 'unixepoch')
                       AND timestamp <= datetime(?, 'unixepoch')
                     ORDER BY timestamp ASC, state_id ASC",
                )?;
                let out = stmt
                    .query_map(params![entity_id, start_ts, end_ts], map_ramp_sample)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                return Ok(out);
            }
            let mut stmt = conn.prepare(
                "SELECT CAST(strftime('%s', timestamp) AS REAL),
                        json_extract(state_json, '$.temperature_current'),
                        json_extract(state_json, '$.temperature_hardware'),
                        json_extract(state_json, '$.temperature_target'),
                        json_extract(state_json, '$.window_open')
                 FROM state_history WHERE entity_id = ?
                 ORDER BY timestamp DESC, state_id DESC LIMIT ?",
            )?;
            let mut rev = stmt
                .query_map(
                    params![entity_id, HEATING_RAMP_HISTORY_LIMIT],
                    map_ramp_sample,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rev.reverse();
            Ok(rev)
        })
    }
}

// =============================================================================
/// Maps one SQLite row directly into a [`RampSample`].
fn map_ramp_sample(r: &rusqlite::Row<'_>) -> rusqlite::Result<RampSample> {
    let ts: Option<f64> = r.get(0)?;
    let temp: Option<f64> = r.get(1)?;
    let hw: Option<f64> = r.get(2)?;
    let target: Option<f64> = r.get(3)?;
    let win: Option<i64> = r.get(4)?;
    Ok(RampSample {
        ts: ts.unwrap_or(-1.0),
        temp: temp.unwrap_or(0.0),
        hw_temp: hw.filter(|&v| v > 0.0),
        target: target.unwrap_or(0.0),
        window_open: win.unwrap_or(0) != 0,
    })
}

// =============================================================================
/// Checks whether active radiator hot-water circulation exists between two JSON state maps.
#[must_use]
pub(super) fn has_active_water_circulation(
    prev: &Map<String, Value>,
    curr: &Map<String, Value>,
) -> bool {
    let p_in = prev
        .get("temperature_current")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let c_in = curr
        .get("temperature_current")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let p_hw = prev
        .get("temperature_hardware")
        .and_then(Value::as_f64)
        .filter(|&v| v > 0.0);
    let c_hw = curr
        .get("temperature_hardware")
        .and_then(Value::as_f64)
        .filter(|&v| v > 0.0);
    circulation_between_temps(p_in, p_hw, c_in, c_hw)
}

// =============================================================================
/// Returns at most the trailing `max_len` items of `items`.
fn tail_slice(items: &[Value], max_len: usize) -> Vec<Value> {
    let start = items.len().saturating_sub(max_len);
    items[start..].to_vec()
}
