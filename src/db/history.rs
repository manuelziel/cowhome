//! Column-oriented time-series history retrieval (`GET /api/v1/history`) & window slicing.
//!
//! Formats `state_history` snapshots into compact `columns` + `points` 2D numeric arrays,
//! attaches inline `efficiency`, `heating_forecast`, or `energy_summary` blocks, and
//! provides pre-window baseline row loading for step-before thermal/energy integration.

use std::collections::HashMap;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use super::efficiency::AnalyticsContext;
use super::Database;
use crate::core::domain_constants::{
    analytics::{
        efficiency_defaults::SECONDS_PER_DAY,
        history_defaults::{
            DEFAULT_RANGE, MAX_LIMIT, RANGE_ALL, ROUTER_COLUMNS, SENSOR_COLUMNS, SUPPORTED_RANGES,
            SWITCH_COLUMNS, THERMOSTAT_COLUMNS, WEATHER_COLUMNS,
        },
    },
    automation::{sync_defaults::MILLISECOND_THRESHOLD, weather_defaults::UNKNOWN_WEATHER_CODE},
    hardware::{device_types, fritz_api::DEFAULT_BATTERY_LEVEL, special_devices::GROUP_ID_PREFIX},
};
use crate::core::error::{AppError, AppResult};
use crate::core::heating_forecast::ForecastDayInput;
use crate::core::time_util::{normalize_client_timestamp, now_epoch_f64};
use crate::models::group::format_group_id;
use crate::models::registry::LiveSwitchTelemetry;

// =============================================================================
/// Parameter bundle for `GET /api/v1/history` queries on single devices or software groups.
#[derive(Debug, Clone)]
pub struct HistoryQuery<'a> {
    /// Device AIN (`"09995 0123456"`, `"WEATHER_01"`, `"ROUTER_01"`) or `"group-<id>"`.
    pub ain: &'a str,
    /// Optional lower-bound UNIX timestamp (seconds or milliseconds).
    pub since: Option<i64>,
    /// Optional named range (`"24h"`, `"7d"`, `"30d"`, `"90d"`, `"180d"`, `"365d"`, `"1y"`, `"all"`).
    pub range_str: Option<&'a str>,
    /// Maximum number of points (`0` = unlimited, positive capped to [`MAX_LIMIT`]).
    pub limit: usize,
    /// Optional 5-day Open-Meteo daily forecast for inline `heating_forecast`.
    pub daily_forecast: &'a [ForecastDayInput],
    /// Optional map of normalized AIN -> live target temperature (`°C`).
    pub live_targets: Option<&'a HashMap<String, f64>>,
    /// Optional home IANA timezone name for local midnight energy slicing.
    pub tz_name: Option<&'a str>,
    /// Optional map of normalized AIN -> live smart-switch telemetry.
    pub live_switches: Option<&'a HashMap<String, LiveSwitchTelemetry>>,
    /// Optional live outdoor temperature (`°C`) from `WeatherService`.
    pub live_outdoor_temp: Option<f64>,
}

impl Database {
    /// Fetches column-oriented history points and inline analytics for a device or group.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on unsupported `range`, [`AppError::NotFound`] on
    /// unknown device/group, or [`AppError::Database`] on SQLite query failure.
    pub fn get_device_history(&self, q: &HistoryQuery<'_>) -> AppResult<Value> {
        let trimmed = q.ain.trim();
        let norm_since = normalize_since_seconds(q.since);
        if trimmed.starts_with(GROUP_ID_PREFIX) {
            return self.get_group_history(trimmed, q, norm_since);
        }
        let range_mod = resolve_sql_range_modifier(norm_since, q.range_str)?;
        let entity = self.resolve_entity(trimmed)?;
        let rows = self.with_conn(|conn| {
            query_raw_history_rows(conn, entity.entity_id, norm_since, range_mod, q.limit)
        })?;
        let (columns, points) = format_history_points(&entity.device_type, &rows);
        let count = points.len();
        let mut out = json!({
            "device": entity.ain,
            "device_type": entity.device_type,
            "columns": columns,
            "points": points,
            "count": count,
        });
        self.attach_history_analytics(&mut out, &entity.ain, &entity.device_type, q);
        Ok(out)
    }

    /// Attaches inline `efficiency`/`heating_forecast` or `energy_summary` onto a history response.
    fn attach_history_analytics(
        &self,
        out: &mut Value,
        ain: &str,
        device_type: &str,
        q: &HistoryQuery<'_>,
    ) {
        let Some(obj) = out.as_object_mut() else {
            return;
        };
        if matches!(
            device_type,
            device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
        ) {
            let ctx = AnalyticsContext {
                since: q.since,
                range_str: q.range_str,
                daily_forecast: q.daily_forecast,
                live_targets: q.live_targets,
                live_outdoor_temp: q.live_outdoor_temp,
            };
            let eff = self.get_heating_efficiency(ain, &ctx);
            let fc = eff
                .as_ref()
                .and_then(|v| v.get("heating_forecast"))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert(String::from("efficiency"), eff.unwrap_or(Value::Null));
            obj.insert(String::from("heating_forecast"), fc);
        } else if device_type == device_types::SWITCH {
            let summary = self
                .get_energy_consumption_summary(ain, q.tz_name, None, q.live_switches)
                .and_then(|s| serde_json::to_value(s).ok())
                .unwrap_or(Value::Null);
            obj.insert(String::from("energy_summary"), summary);
        }
    }

    /// Fetches representative history points + aggregated analytics for a virtual software group.
    fn get_group_history(
        &self,
        group_id: &str,
        q: &HistoryQuery<'_>,
        norm_since: Option<i64>,
    ) -> AppResult<Value> {
        let grp = self
            .get_software_group(group_id)?
            .ok_or_else(|| AppError::NotFound(format!("Software group '{group_id}' not found")))?;
        let range_mod = resolve_sql_range_modifier(norm_since, q.range_str)?;
        let gid_num = grp
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let gid_prefixed = format_group_id(gid_num);
        let members: Vec<String> = grp
            .get("members")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let (columns, points) =
            self.find_group_member_points(&members, norm_since, range_mod, q.limit)?;
        let ctx = AnalyticsContext {
            since: norm_since,
            range_str: q.range_str,
            daily_forecast: q.daily_forecast,
            live_targets: q.live_targets,
            live_outdoor_temp: q.live_outdoor_temp,
        };
        let eff = self.aggregate_efficiency_for_ains(&members, &ctx);
        let fc = eff
            .as_ref()
            .and_then(|v| v.get("heating_forecast"))
            .cloned()
            .unwrap_or(Value::Null);
        let energy = self
            .aggregate_energy_for_ains(&members, &gid_prefixed, q.tz_name, None, q.live_switches)
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(Value::Null);

        Ok(json!({
            "device": gid_prefixed,
            "device_type": device_types::THERMOSTAT_GROUP,
            "columns": columns,
            "points": points,
            "count": points.len(),
            "efficiency": eff,
            "heating_forecast": fc,
            "energy_summary": energy,
        }))
    }

    /// Selects columnar points from the first group member holding thermostat or switch data.
    fn find_group_member_points(
        &self,
        members: &[String],
        since: Option<i64>,
        range_mod: Option<&'static str>,
        limit: usize,
    ) -> AppResult<(Vec<&'static str>, Vec<Value>)> {
        let mut columns = THERMOSTAT_COLUMNS.to_vec();
        for fmt_type in [device_types::THERMOSTAT_GROUP, device_types::SWITCH] {
            for member_ain in members {
                let Ok(ent) = self.resolve_entity(member_ain) else {
                    continue;
                };
                let matches = if fmt_type == device_types::THERMOSTAT_GROUP {
                    matches!(
                        ent.device_type.as_str(),
                        device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
                    )
                } else {
                    ent.device_type == fmt_type
                };
                if !matches {
                    continue;
                }
                let rows = self.with_conn(|conn| {
                    query_raw_history_rows(conn, ent.entity_id, since, range_mod, limit)
                })?;
                let (cols, pts) = format_history_points(fmt_type, &rows);
                columns = cols;
                if !pts.is_empty() {
                    return Ok((columns, pts));
                }
            }
        }
        Ok((columns, Vec::new()))
    }

    /// Loads chronological `(timestamp_secs, state_map)` rows with a clamped pre-window baseline.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `state_history` fails.
    pub fn fetch_bounded_history_rows(
        &self,
        entity_id: i64,
        start_ts: f64,
        end_ts: f64,
    ) -> AppResult<Vec<(f64, Map<String, Value>)>> {
        self.with_conn(|conn| {
            let mut out = Vec::new();
            let mut base_stmt = conn.prepare(
                "SELECT CAST(strftime('%s', timestamp) AS REAL), state_json FROM state_history
                 WHERE entity_id = ? AND timestamp < datetime(?, 'unixepoch')
                 ORDER BY timestamp DESC, state_id DESC LIMIT 1",
            )?;
            for item in base_stmt.query_map(params![entity_id, start_ts], |r| {
                Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, String>(1)?))
            })? {
                let (opt_ts, raw) = item?;
                if let (Some(ts), Ok(Value::Object(map))) = (opt_ts, serde_json::from_str(&raw)) {
                    out.push((ts.max(start_ts), map));
                }
            }
            let mut win_stmt = conn.prepare(
                "SELECT CAST(strftime('%s', timestamp) AS REAL), state_json FROM state_history
                 WHERE entity_id = ? AND timestamp >= datetime(?, 'unixepoch')
                   AND timestamp <= datetime(?, 'unixepoch')
                 ORDER BY timestamp ASC, state_id ASC",
            )?;
            for item in win_stmt.query_map(params![entity_id, start_ts, end_ts], |r| {
                Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, String>(1)?))
            })? {
                let (opt_ts, raw) = item?;
                if let (Some(ts), Ok(Value::Object(map))) = (opt_ts, serde_json::from_str(&raw)) {
                    out.push((ts.max(start_ts), map));
                }
            }
            Ok(out)
        })
    }
}

// =============================================================================
/// Normalizes `since` from milliseconds (`>= 100_000_000_000`) to UNIX seconds per `openapi.yaml`.
#[must_use]
fn normalize_since_seconds(since: Option<i64>) -> Option<i64> {
    since.map(|raw| {
        if (raw as f64) >= MILLISECOND_THRESHOLD {
            raw / 1000
        } else {
            raw
        }
    })
}

// =============================================================================
/// Resolves `(start_ts, end_ts)` in UNIX seconds for efficiency & ramp window evaluation.
#[must_use]
pub fn resolve_efficiency_window(since: Option<i64>, range_str: Option<&str>) -> (f64, f64) {
    let now_ts = now_epoch_f64();
    if let Some(s) = since.filter(|&v| v > 0) {
        let s_val = json!(s);
        let eff = normalize_client_timestamp(Some(&s_val), None, Some(now_ts))
            .map_or(now_ts - SECONDS_PER_DAY, |(ts, _)| ts);
        return (eff, now_ts);
    }
    let key = range_str
        .unwrap_or(DEFAULT_RANGE)
        .trim()
        .to_ascii_lowercase();
    if key == RANGE_ALL {
        return (0.0, now_ts);
    }
    let span = match key.as_str() {
        "7d" => 7.0 * SECONDS_PER_DAY,
        "30d" => 30.0 * SECONDS_PER_DAY,
        "90d" => 90.0 * SECONDS_PER_DAY,
        "180d" => 180.0 * SECONDS_PER_DAY,
        "365d" | "1y" => 365.0 * SECONDS_PER_DAY,
        _ => SECONDS_PER_DAY,
    };
    (now_ts - span, now_ts)
}

// =============================================================================
/// Maps `range_str` into SQLite's `datetime('now', <modifier>)` literal, validating input.
fn resolve_sql_range_modifier(
    since: Option<i64>,
    range_str: Option<&str>,
) -> AppResult<Option<&'static str>> {
    if since.is_some() {
        return Ok(None);
    }
    let raw = range_str.unwrap_or(DEFAULT_RANGE).trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case(RANGE_ALL) {
        return Ok(None);
    }
    match raw.to_ascii_lowercase().as_str() {
        "24h" => Ok(Some("-24 hours")),
        "7d" => Ok(Some("-7 days")),
        "30d" => Ok(Some("-30 days")),
        "90d" => Ok(Some("-90 days")),
        "180d" => Ok(Some("-180 days")),
        "365d" => Ok(Some("-365 days")),
        "1y" => Ok(Some("-1 year")),
        _ => Err(AppError::Validation(format!(
            "Invalid range '{raw}'. Supported: {}",
            SUPPORTED_RANGES.join(", ")
        ))),
    }
}

// =============================================================================
/// Queries chronological `(unix_ts_i64, state_json)` rows for `entity_id`.
fn query_raw_history_rows(
    conn: &Connection,
    entity_id: i64,
    since: Option<i64>,
    range_mod: Option<&'static str>,
    limit: usize,
) -> AppResult<Vec<(i64, String)>> {
    let eff_limit = if limit == 0 {
        i64::MAX
    } else {
        i64::try_from(limit.min(MAX_LIMIT)).unwrap_or(MAX_LIMIT as i64)
    };
    if let Some(since_ts) = since {
        let mut stmt = conn.prepare(
            "SELECT CAST(strftime('%s', timestamp) AS INTEGER), state_json FROM state_history
             WHERE entity_id = ? AND timestamp > datetime(?, 'unixepoch')
             ORDER BY timestamp ASC, state_id ASC LIMIT ?",
        )?;
        let rows = collect_ts_json_rows(
            stmt.query_map(params![entity_id, since_ts, eff_limit], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?,
        )?;
        return Ok(rows);
    }
    if let Some(modifier) = range_mod {
        let mut stmt = conn.prepare(
            "SELECT CAST(strftime('%s', timestamp) AS INTEGER), state_json FROM state_history
             WHERE entity_id = ? AND timestamp >= datetime('now', ?)
             ORDER BY timestamp ASC, state_id ASC LIMIT ?",
        )?;
        let rows = collect_ts_json_rows(
            stmt.query_map(params![entity_id, modifier, eff_limit], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?,
        )?;
        return Ok(rows);
    }
    let mut stmt = conn.prepare(
        "SELECT CAST(strftime('%s', timestamp) AS INTEGER), state_json FROM state_history
         WHERE entity_id = ? ORDER BY timestamp ASC, state_id ASC LIMIT ?",
    )?;
    let rows = collect_ts_json_rows(stmt.query_map(params![entity_id, eff_limit], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?)?;
    Ok(rows)
}

// =============================================================================
/// Drains a `rusqlite::MappedRows` iterator of `(Option<i64>, String)` into `Vec<(i64, String)>`.
fn collect_ts_json_rows<I>(iter: I) -> AppResult<Vec<(i64, String)>>
where
    I: Iterator<Item = rusqlite::Result<(Option<i64>, String)>>,
{
    let mut out = Vec::new();
    for item in iter {
        let (ts_opt, raw) = item?;
        if let Some(ts) = ts_opt {
            out.push((ts, raw));
        }
    }
    Ok(out)
}

// =============================================================================
/// Formats raw `state_history` rows into `(columns, 2D_points)` matching `openapi.yaml`.
fn format_history_points(
    device_type: &str,
    rows: &[(i64, String)],
) -> (Vec<&'static str>, Vec<Value>) {
    let cols = match device_type {
        device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP => THERMOSTAT_COLUMNS,
        device_types::SWITCH => SWITCH_COLUMNS,
        device_types::SENSOR => SENSOR_COLUMNS,
        device_types::WEATHER => WEATHER_COLUMNS,
        _ => ROUTER_COLUMNS,
    };
    let points = rows
        .iter()
        .map(|(ts, raw)| {
            let map = serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            format_single_point(device_type, *ts, &map)
        })
        .collect();
    (cols.to_vec(), points)
}

// =============================================================================
/// Formats a single JSON state map into its typed columnar array row.
fn format_single_point(device_type: &str, ts: i64, s: &Map<String, Value>) -> Value {
    let bat = s
        .get("battery_level")
        .and_then(Value::as_i64)
        .unwrap_or(i64::from(DEFAULT_BATTERY_LEVEL));
    let t_cur = num_or_zero(s, "temperature_current");
    let hum = num_or_zero(s, "humidity");

    match device_type {
        device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP => {
            let t_tgt = num_or_zero(s, "temperature_target");
            let t_hw = s
                .get("temperature_hardware")
                .and_then(Value::as_f64)
                .unwrap_or(t_cur);
            let hw_tgt = s
                .get("hardware_target_temperature")
                .and_then(Value::as_f64)
                .unwrap_or(t_tgt);
            json!([
                ts,
                t_cur,
                t_tgt,
                hum,
                bat,
                bool_flag(s, "window_open"),
                t_hw,
                hw_tgt,
                bool_flag(s, "humidity_guard_active"),
                bool_flag(s, "preheat_active"),
            ])
        }
        device_types::SWITCH => json!([
            ts,
            bool_flag(s, "state"),
            num_or_zero(s, "power_current"),
            num_or_zero(s, "energy_total"),
            t_cur,
            num_or_zero(s, "voltage"),
        ]),
        device_types::SENSOR => json!([ts, t_cur, hum, bat]),
        device_types::WEATHER => {
            let w_code = s
                .get("weather_code")
                .and_then(Value::as_i64)
                .unwrap_or(i64::from(UNKNOWN_WEATHER_CODE));
            json!([
                ts,
                num_or_zero(s, "temperature"),
                num_or_zero(s, "apparent_temperature"),
                hum,
                num_or_zero(s, "wind_speed"),
                w_code,
            ])
        }
        _ => json!([ts, bool_flag(s, "present")]),
    }
}

// =============================================================================
/// Extracts a floating-point field from `map`, defaulting to `0.0`.
fn num_or_zero(map: &Map<String, Value>, key: &str) -> f64 {
    map.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

// =============================================================================
/// Extracts a boolean field from `map` as `1` (`true`) or `0` (`false`).
fn bool_flag(map: &Map<String, Value>, key: &str) -> i64 {
    i64::from(map.get(key).and_then(Value::as_bool).unwrap_or(false))
}
