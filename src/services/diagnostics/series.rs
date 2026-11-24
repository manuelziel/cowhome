//! Semantic time-series curve summarization & offline Digital Twin fallback helpers.
//!
//! Computes `OTel`-aligned metric summaries (`start`, `end`, `min`, `max`, `avg`, `delta`),
//! discrete state transitions, target-ID filtering, and SQLite offline snapshots.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::core::domain_constants::{
    analytics::history_defaults::DEFAULT_LIMIT,
    hardware::{
        device_types,
        fritz_api::{
            DEFAULT_THERMOSTAT_OFFSET, SUBUNIT_DEFAULT_SUFFIX, thermostat_error_description,
        },
        special_devices::{GROUP_ID_PREFIX, WEATHER_AIN},
    },
};
use crate::core::heating_forecast::ForecastDayInput;
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::db::history::HistoryQuery;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;

/// Maximum discrete state transitions included per entity curve summary (`30`).
const MAX_TRANSITIONS: usize = 30;

/// Numeric telemetry columns summarized statistically in `curve_summary.metrics`.
const WATCHED_METRICS: &[&str] = &[
    "temperature_current",
    "temperature_hardware",
    "temperature_target",
    "humidity",
    "power_current",
    "energy_total",
    "voltage",
];

/// Discrete state columns tracked in `curve_summary.state_transitions`.
const WATCHED_TRANSITIONS: &[&str] = &[
    "temperature_target",
    "hardware_target_temperature",
    "state",
    "window_open",
    "humidity_guard_active",
    "preheat_active",
];

// =============================================================================
/// Serializes a live [`DeviceState`] including transient `target_hold_until` & `offset_control_active`.
#[must_use]
pub(super) fn serialize_live_device(device: &DeviceState) -> Value {
    let mut val = serde_json::to_value(device).unwrap_or_else(|_| json!({}));
    if let (Some(obj), Some(t)) = (val.as_object_mut(), device.as_thermostat()) {
        obj.insert(
            String::from("target_hold_until"),
            json!(round_decimals(t.target_hold_until, 3)),
        );
        obj.insert(
            String::from("offset_control_active"),
            json!(t.offset_control_active),
        );
    }
    val
}

// =============================================================================
/// Reconstructs latest persisted device states from SQLite when no live daemon is attached.
#[must_use]
pub(super) fn load_offline_devices_from_db(db: &Database) -> Vec<Value> {
    let rows: Vec<(String, String, String)> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ain, name, device_type FROM entities
                 WHERE ain != ? ORDER BY entity_id ASC",
            )?;
            let mapped = stmt
                .query_map([WEATHER_AIN], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(mapped)
        })
        .unwrap_or_default();

    let norm_set: HashSet<String> = rows.iter().map(|(a, _, _)| normalize_ain(a)).collect();
    let mut out = Vec::with_capacity(rows.len());
    for (ain, name, dtype) in rows {
        if dtype == device_types::GENERIC
            && norm_set.contains(&format!("{}{SUBUNIT_DEFAULT_SUFFIX}", normalize_ain(&ain)))
        {
            continue;
        }
        let mut state_map = db
            .get_latest_state_snapshot(&ain)
            .ok()
            .flatten()
            .map(|(m, _)| m)
            .unwrap_or_default();
        state_map
            .entry(String::from("ain"))
            .or_insert_with(|| json!(ain));
        state_map
            .entry(String::from("device_type"))
            .or_insert_with(|| json!(dtype));
        if !name.is_empty() {
            state_map
                .entry(String::from("name"))
                .or_insert_with(|| json!(name));
        }
        let is_thermo = matches!(
            state_map
                .get("device_type")
                .and_then(Value::as_str)
                .unwrap_or(""),
            device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
        ) || state_map.contains_key("error_code");
        if is_thermo {
            enrich_offline_thermostat(db, &ain, &mut state_map);
        }
        out.push(Value::Object(state_map));
    }
    out
}

// =============================================================================
/// Resolves user `device_filter` (`None` / `"all"` / `"group-1"` / AIN / display name) to target IDs.
#[must_use]
pub(super) fn resolve_target_identifiers(twin: &Value, device_filter: Option<&str>) -> Vec<String> {
    let devices = twin
        .get("devices")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let groups = twin
        .get("software_groups")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let needle = device_filter.unwrap_or("").trim();

    if needle.is_empty() || needle.eq_ignore_ascii_case("all") {
        let mut ids: Vec<String> = groups
            .iter()
            .filter_map(|g| g.get("id").and_then(Value::as_str))
            .map(|id| format!("{GROUP_ID_PREFIX}{id}"))
            .collect();
        ids.extend(
            devices
                .iter()
                .filter(|d| {
                    matches!(
                        d.get("device_type").and_then(Value::as_str).unwrap_or(""),
                        device_types::THERMOSTAT
                            | device_types::THERMOSTAT_GROUP
                            | device_types::SWITCH
                    )
                })
                .filter_map(|d| d.get("ain").and_then(Value::as_str))
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
        );
        return ids;
    }

    let needle_low = needle.to_ascii_lowercase();
    let needle_norm = normalize_ain(&needle_low);
    for grp in groups {
        let raw_id = grp.get("id").and_then(Value::as_str).unwrap_or("");
        let gid = format!("{GROUP_ID_PREFIX}{raw_id}");
        let gname = grp
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if needle_low == gid.to_ascii_lowercase()
            || needle_low == raw_id.to_ascii_lowercase()
            || needle_low == gname
        {
            let mut out = vec![gid];
            if let Some(mems) = grp.get("members").and_then(Value::as_array) {
                out.extend(
                    mems.iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                );
            }
            return out;
        }
    }
    for dev in devices {
        let ain = dev.get("ain").and_then(Value::as_str).unwrap_or("");
        let dname = dev
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if needle_low == dname || needle_norm == normalize_ain(&ain.to_ascii_lowercase()) {
            return vec![ain.to_owned()];
        }
    }
    vec![needle.to_owned()]
}

// =============================================================================
/// Generates `curve_summary`, `efficiency`, `heating_forecast`, `energy_summary`, & `ramp_diagnostics`.
pub(super) fn analyze_single_entity(
    identifier: &str,
    range_str: &str,
    db: Option<&Database>,
    registry: Option<&StateRegistry>,
    daily_forecast: &[ForecastDayInput],
    tz_name: &str,
    live_outdoor_temp: Option<f64>,
) -> Value {
    let Some(database) = db else {
        return json!({ "device": identifier, "error": "Database unavailable" });
    };
    let live_targets = registry.map(StateRegistry::get_live_thermostat_targets);
    let live_switches = registry.map(StateRegistry::get_live_switch_states);
    let q = HistoryQuery {
        ain: identifier,
        since: None,
        range_str: Some(range_str),
        limit: DEFAULT_LIMIT,
        daily_forecast,
        live_targets: live_targets.as_ref(),
        tz_name: Some(tz_name),
        live_switches: live_switches.as_ref(),
        live_outdoor_temp,
    };
    let hist = match database.get_device_history(&q) {
        Ok(h) => h,
        Err(err) => {
            return json!({
                "device": identifier,
                "range": range_str,
                "error": err.to_string(),
            });
        }
    };
    let cols: Vec<String> = hist
        .get("columns")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let pts = hist
        .get("points")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let summary = summarize_history_points(&cols, pts);

    let dtype = hist
        .get("device_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_group = identifier.to_ascii_lowercase().starts_with(GROUP_ID_PREFIX);
    let ramps = (!is_group
        && matches!(
            dtype,
            device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
        ))
    .then(|| database.get_heating_ramp_diagnostics(identifier, None, Some(range_str)));

    json!({
        "device": hist.get("device").and_then(Value::as_str).unwrap_or(identifier),
        "device_type": hist.get("device_type").cloned().unwrap_or(Value::Null),
        "range": range_str,
        "curve_summary": summary,
        "efficiency": hist.get("efficiency").cloned().unwrap_or(Value::Null),
        "heating_forecast": hist.get("heating_forecast").cloned().unwrap_or(Value::Null),
        "energy_summary": hist.get("energy_summary").cloned().unwrap_or(Value::Null),
        "ramp_diagnostics": ramps,
    })
}

// =============================================================================
/// Summarizes columnar `points` into min/max/avg/delta stats and discrete transitions.
fn summarize_history_points(columns: &[String], points: &[Value]) -> Value {
    if points.is_empty() || columns.is_empty() {
        return json!({ "sample_count": 0, "metrics": {}, "state_transitions": [] });
    }
    let col_idx: HashMap<&str, usize> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();

    let mut metrics = Map::new();
    for &metric in WATCHED_METRICS {
        let Some(&idx) = col_idx.get(metric) else {
            continue;
        };
        let vals: Vec<f64> = points
            .iter()
            .filter_map(Value::as_array)
            .filter_map(|row| row.get(idx)?.as_f64())
            .collect();
        if let (Some(&first), Some(&last)) = (vals.first(), vals.last()) {
            let min_v = vals.iter().copied().reduce(f64::min).unwrap_or(first);
            let max_v = vals.iter().copied().reduce(f64::max).unwrap_or(first);
            let avg_v = vals.iter().sum::<f64>() / (vals.len() as f64);
            metrics.insert(
                metric.to_owned(),
                json!({
                    "start": round_decimals(first, 2),
                    "end": round_decimals(last, 2),
                    "min": round_decimals(min_v, 2),
                    "max": round_decimals(max_v, 2),
                    "avg": round_decimals(avg_v, 2),
                    "delta": round_decimals(last - first, 2),
                }),
            );
        }
    }

    let transitions = extract_transitions(&col_idx, points);
    let tail_start = transitions.len().saturating_sub(MAX_TRANSITIONS);
    let first_ts = points
        .first()
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null);
    let last_ts = points
        .last()
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "sample_count": points.len(),
        "first_timestamp": first_ts,
        "last_timestamp": last_ts,
        "metrics": metrics,
        "state_transitions": &transitions[tail_start..],
    })
}

// =============================================================================
/// Extracts discrete setpoint, relay, window, and guard state transitions across `points`.
fn extract_transitions(col_idx: &HashMap<&str, usize>, points: &[Value]) -> Vec<Value> {
    let active_cols: Vec<(&str, usize)> = WATCHED_TRANSITIONS
        .iter()
        .filter_map(|&k| col_idx.get(k).map(|&idx| (k, idx)))
        .collect();
    let mut transitions = Vec::new();
    let mut prev_vals: HashMap<&str, Value> = HashMap::new();

    for row_val in points {
        let Some(row) = row_val.as_array() else {
            continue;
        };
        let ts = row.first().cloned().unwrap_or(Value::Null);
        for &(key, idx) in &active_cols {
            let Some(val) = row.get(idx).filter(|v| !v.is_null()) else {
                continue;
            };
            if let Some(prev) = prev_vals.get(key)
                && prev != val
            {
                transitions.push(json!({
                    "timestamp": ts,
                    "field": key,
                    "from": prev,
                    "to": val,
                }));
            }
            prev_vals.insert(key, val.clone());
        }
    }
    transitions
}

// =============================================================================
/// Populates AVM error description, offsets, and `Pre-Heat` defaults on offline snapshots.
fn enrich_offline_thermostat(db: &Database, ain: &str, state: &mut Map<String, Value>) {
    let err_code = state.get("error_code").and_then(Value::as_u64).unwrap_or(0);
    state.insert(String::from("error_code"), json!(err_code));
    #[allow(clippy::cast_possible_truncation)]
    state
        .entry(String::from("error_description"))
        .or_insert_with(|| json!(thermostat_error_description(err_code as u8)));
    state.insert(
        String::from("temperature_offset"),
        json!(db.get_thermostat_offset(ain)),
    );
    state
        .entry(String::from("fritz_temperature_offset"))
        .or_insert_with(|| json!(DEFAULT_THERMOSTAT_OFFSET));
    state
        .entry(String::from("preheat_active"))
        .or_insert_with(|| json!(false));
    state
        .entry(String::from("preheat_target_temp"))
        .or_insert(Value::Null);
    state
        .entry(String::from("preheat_target_time"))
        .or_insert(Value::Null);
    state
        .entry(String::from("preheat_paused_reason"))
        .or_insert(Value::Null);
}
