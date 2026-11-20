//! Time-series state recorder with Deadband compression, `30 min` Heartbeat, & Pre-Edge Anchor.
//!
//! Minimizes SD/NVMe flash wear by suppressing redundant `state_history` writes while
//! inserting exact `Pre-Edge` step-before anchor points (`>= 120 s` gap) right before state
//! transitions so thermal and energy integrals remain distortion-free.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

use super::entities_sync::resolve_entity_on;
use super::Database;
use crate::core::domain_constants::{
    analytics::history_defaults::{
        DEADBAND_BATTERY_PCT, DEADBAND_ENERGY_WH, DEADBAND_HUMIDITY_PCT, DEADBAND_POWER_RATIO,
        DEADBAND_POWER_WATTS, DEADBAND_TARGET_TEMP_C, DEADBAND_TEMP_C, DEADBAND_VOLTAGE_V,
        FLOAT_EPSILON, HEARTBEAT_INTERVAL_SECS, PRE_EDGE_MIN_GAP_SECS,
    },
    hardware::{
        device_types,
        special_devices::{WEATHER_AIN, WEATHER_NAME},
    },
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::now_epoch_f64;
use crate::models::device::DeviceState;

/// Discrete JSON state keys triggering an immediate snapshot whenever their value changes.
const DISCRETE_STATE_KEYS: &[&str] = &[
    "device_type",
    "present",
    "state",
    "window_open",
    "boost_active",
    "humidity_guard_active",
    "preheat_active",
    "battery_low",
    "error_code",
    "last_pressed",
];

/// Numeric JSON state keys and their absolute deadband thresholds.
const NUMERIC_DEADBANDS: &[(&str, f64)] = &[
    ("temperature_current", DEADBAND_TEMP_C),
    ("temperature_hardware", DEADBAND_TEMP_C),
    ("temperature_target", DEADBAND_TARGET_TEMP_C),
    ("hardware_target_temperature", DEADBAND_TARGET_TEMP_C),
    ("comfort_temperature", DEADBAND_TARGET_TEMP_C),
    ("eco_temperature", DEADBAND_TARGET_TEMP_C),
    ("temperature_offset", DEADBAND_TARGET_TEMP_C),
    ("fritz_temperature_offset", DEADBAND_TARGET_TEMP_C),
    ("humidity", DEADBAND_HUMIDITY_PCT),
    ("battery_level", DEADBAND_BATTERY_PCT),
    ("voltage", DEADBAND_VOLTAGE_V),
    ("energy_total", DEADBAND_ENERGY_WH),
];

// =============================================================================
/// In-memory deadband, heartbeat, and pre-edge suppression state owned by [`Database`].
#[derive(Debug, Default)]
pub(super) struct RecorderCaches {
    /// Last persisted state map (excluding `last_updated`) and epoch timestamp per AIN.
    pub state_saved: HashMap<String, (Map<String, Value>, f64)>,
    /// Most recent suppressed full JSON payload and epoch timestamp per AIN.
    pub state_suppressed: HashMap<String, (Value, f64)>,
    /// Last persisted presence flag and epoch timestamp per lowercased MAC address.
    pub presence_saved: HashMap<String, (bool, f64)>,
    /// Most recent suppressed presence flag and epoch timestamp per lowercased MAC address.
    pub presence_suppressed: HashMap<String, (bool, f64)>,
}

impl RecorderCaches {
    /// Evicts cached presence baselines older than `cutoff_ts` after `prune_history`.
    pub(super) fn evict_presence_older_than(&mut self, cutoff_ts: f64) {
        self.presence_saved
            .retain(|_, &mut (_, ts)| ts >= cutoff_ts);
        self.presence_suppressed
            .retain(|_, &mut (_, ts)| ts >= cutoff_ts);
    }
}

impl Database {
    /// Inserts a JSON state snapshot into `state_history` at `timestamp` (or `CURRENT_TIMESTAMP`).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if JSON serialization or SQL insertion fails.
    pub fn insert_state_history(
        &self,
        entity_id: i64,
        state_val: &Value,
        timestamp: Option<f64>,
    ) -> AppResult<()> {
        self.with_conn(|conn| insert_state_history_on(conn, entity_id, state_val, timestamp))
    }

    /// Retrieves the latest `(state_map_without_last_updated, unix_ts)` for an AIN.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the SQLite query fails (`None` when entity is unknown).
    pub fn get_latest_state_snapshot(
        &self,
        ain: &str,
    ) -> AppResult<Option<(Map<String, Value>, f64)>> {
        self.with_conn(|conn| {
            let Ok(entity) = resolve_entity_on(conn, ain) else {
                return Ok(None);
            };
            query_latest_snapshot_on(conn, entity.entity_id)
        })
    }

    /// Retrieves the latest `(state_map_without_last_updated, unix_ts)` by numeric `entity_id`.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the SQLite query fails.
    pub fn get_latest_state_snapshot_by_id(
        &self,
        entity_id: i64,
    ) -> AppResult<Option<(Map<String, Value>, f64)>> {
        self.with_conn(|conn| query_latest_snapshot_on(conn, entity_id))
    }

    /// Loads (and caches) the initial persisted comparison baseline for `state` on worker startup.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if entity registration or baseline lookup fails.
    pub fn load_initial_device_baseline(
        &self,
        state: &DeviceState,
    ) -> AppResult<Option<Map<String, Value>>> {
        self.with_conn(|conn| {
            self.ensure_device_baseline_on(conn, &state.ain, &state.name, &state.device_type)
        })
        .map(|opt| opt.map(|(map, _)| map))
    }

    /// Persists `state` using Deadband compression, `30 min` Heartbeat, and Pre-Edge Anchor.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if entity registration or history insertion fails.
    pub fn persist_device_history(
        &self,
        state: &DeviceState,
        now_ts: Option<f64>,
    ) -> AppResult<()> {
        let eff_now = now_ts.unwrap_or_else(now_epoch_f64);
        self.with_tx(|tx| self.persist_device_history_on(tx, state, Some(eff_now)))
    }

    /// Persists `state` on an existing connection/transaction (for single-tx poll cycle batching).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if serialization, registration, or insertion fails.
    pub fn persist_device_history_on(
        &self,
        conn: &Connection,
        state: &DeviceState,
        now_ts: Option<f64>,
    ) -> AppResult<()> {
        let eff_now = now_ts.unwrap_or_else(now_epoch_f64);
        let curr = serde_json::to_value(state)
            .map_err(|e| AppError::Database(format!("DeviceState JSON serialize failed: {e}")))?;
        let Some(curr_obj) = curr.as_object() else {
            return Ok(());
        };
        let mut compare = curr_obj.clone();
        compare.remove("last_updated");

        let baseline =
            self.ensure_device_baseline_on(conn, &state.ain, &state.name, &state.device_type)?;
        let Some((prev_map, last_ts)) = baseline else {
            return self.commit_device_snapshot_on(conn, state, curr, compare, eff_now, false);
        };
        let changed = has_significant_state_change(&prev_map, &compare);
        let hb_due = (eff_now - last_ts) >= (HEARTBEAT_INTERVAL_SECS - FLOAT_EPSILON);
        if !changed && !hb_due {
            if let Ok(mut caches) = self.inner.recorder_caches.lock() {
                caches
                    .state_suppressed
                    .insert(state.ain.clone(), (curr, eff_now));
            }
            return Ok(());
        }
        let same_type = prev_map.get("device_type") == compare.get("device_type");
        self.commit_device_snapshot_on(conn, state, curr, compare, eff_now, changed && same_type)
    }

    /// Persists outdoor `WEATHER_01` telemetry on change or `30 min` heartbeat.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if SQLite registration or insertion fails.
    pub fn persist_weather_history(
        &self,
        current_weather: &Value,
        now_ts: Option<f64>,
    ) -> AppResult<()> {
        let Some(curr_obj) = current_weather.as_object() else {
            return Ok(());
        };
        if curr_obj.get("temperature").is_none_or(Value::is_null) {
            return Ok(());
        }
        let eff_now = now_ts.unwrap_or_else(now_epoch_f64);
        self.with_tx(|tx| {
            let prev = self.ensure_device_baseline_on(
                tx,
                WEATHER_AIN,
                WEATHER_NAME,
                device_types::WEATHER,
            )?;
            if let Some((prev_map, last_ts)) = prev {
                let hb_due = (eff_now - last_ts) >= (HEARTBEAT_INTERVAL_SECS - FLOAT_EPSILON);
                if &prev_map == curr_obj && !hb_due {
                    return Ok(());
                }
            }
            let eid =
                self.register_entity_on(tx, WEATHER_AIN, WEATHER_NAME, device_types::WEATHER)?;
            insert_state_history_on(tx, eid, current_weather, Some(eff_now))?;
            if let Ok(mut caches) = self.inner.recorder_caches.lock() {
                caches
                    .state_saved
                    .insert(WEATHER_AIN.to_owned(), (curr_obj.clone(), eff_now));
            }
            Ok(())
        })
    }

    /// Ensures `(comparison_map, last_saved_ts)` is loaded into `recorder_caches.state_saved`.
    fn ensure_device_baseline_on(
        &self,
        conn: &Connection,
        ain: &str,
        name: &str,
        device_type: &str,
    ) -> AppResult<Option<(Map<String, Value>, f64)>> {
        if let Ok(caches) = self.inner.recorder_caches.lock() {
            if let Some(entry) = caches.state_saved.get(ain) {
                return Ok(Some(entry.clone()));
            }
        }
        let eid = self.register_entity_on(conn, ain, name, device_type)?;
        let Some(snapshot) = query_latest_snapshot_on(conn, eid)? else {
            return Ok(None);
        };
        if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            caches.state_saved.insert(ain.to_owned(), snapshot.clone());
        }
        Ok(Some(snapshot))
    }

    /// Writes optional Pre-Edge Anchor + current snapshot and updates `recorder_caches`.
    fn commit_device_snapshot_on(
        &self,
        conn: &Connection,
        state: &DeviceState,
        curr: Value,
        compare: Map<String, Value>,
        now_ts: f64,
        write_pre_edge: bool,
    ) -> AppResult<()> {
        let eid = self.register_entity_on(conn, &state.ain, &state.name, &state.device_type)?;
        let pre_edge = if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            let last_ts = caches.state_saved.get(&state.ain).map_or(0.0, |e| e.1);
            let sup = caches.state_suppressed.remove(&state.ain);
            let min_gap = PRE_EDGE_MIN_GAP_SECS - FLOAT_EPSILON;
            sup.filter(|&(_, s_ts)| write_pre_edge && (s_ts - last_ts) >= min_gap && now_ts > s_ts)
        } else {
            None
        };
        if let Some((sup_val, sup_ts)) = pre_edge {
            insert_state_history_on(conn, eid, &sup_val, Some(sup_ts))?;
        }
        insert_state_history_on(conn, eid, &curr, Some(now_ts))?;
        if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            caches
                .state_saved
                .insert(state.ain.clone(), (compare, now_ts));
        }
        Ok(())
    }
}

// =============================================================================
/// Inserts a JSON state snapshot into `state_history` on `conn`.
pub(super) fn insert_state_history_on(
    conn: &Connection,
    entity_id: i64,
    state_val: &Value,
    timestamp: Option<f64>,
) -> AppResult<()> {
    let raw_json = serde_json::to_string(state_val)
        .map_err(|e| AppError::Database(format!("Failed serializing state_json: {e}")))?;
    if let Some(ts) = timestamp {
        conn.execute(
            "INSERT INTO state_history (entity_id, state_json, timestamp)
             VALUES (?, ?, datetime(?, 'unixepoch'))",
            params![entity_id, raw_json, ts],
        )?;
    } else {
        conn.execute(
            "INSERT INTO state_history (entity_id, state_json) VALUES (?, ?)",
            params![entity_id, raw_json],
        )?;
    }
    Ok(())
}

// =============================================================================
/// Queries the newest `state_history` row for `entity_id`, stripping `last_updated`.
fn query_latest_snapshot_on(
    conn: &Connection,
    entity_id: i64,
) -> AppResult<Option<(Map<String, Value>, f64)>> {
    let row: Option<(Option<f64>, String)> = conn
        .query_row(
            "SELECT CAST(strftime('%s', timestamp) AS REAL), state_json
             FROM state_history WHERE entity_id = ?
             ORDER BY timestamp DESC, state_id DESC LIMIT 1",
            params![entity_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((Some(ts), raw_json)) = row else {
        return Ok(None);
    };
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&raw_json) else {
        return Ok(None);
    };
    map.remove("last_updated");
    Ok(Some((map, ts)))
}

// =============================================================================
/// Checks whether `curr` differs from `prev` on any discrete flag or numeric/power deadband.
#[must_use]
pub(super) fn has_significant_state_change(
    prev: &Map<String, Value>,
    curr: &Map<String, Value>,
) -> bool {
    for &key in DISCRETE_STATE_KEYS {
        if (prev.contains_key(key) || curr.contains_key(key)) && prev.get(key) != curr.get(key) {
            return true;
        }
    }
    if exceeds_power_deadband(prev, curr) {
        return true;
    }
    for &(key, threshold) in NUMERIC_DEADBANDS {
        if !prev.contains_key(key) && !curr.contains_key(key) {
            continue;
        }
        match (
            value_as_opt_f64(prev.get(key)),
            value_as_opt_f64(curr.get(key)),
        ) {
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => return true,
            (Some(p), Some(c)) if (c - p).abs() >= (threshold - FLOAT_EPSILON) => return true,
            (Some(_), Some(_)) => {}
        }
    }
    false
}

// =============================================================================
/// Evaluates the dual absolute/relative active-power deadband (`max(0.5 W, 2 %)`).
fn exceeds_power_deadband(prev: &Map<String, Value>, curr: &Map<String, Value>) -> bool {
    const KEY: &str = "power_current";
    if !prev.contains_key(KEY) && !curr.contains_key(KEY) {
        return false;
    }
    match (
        value_as_opt_f64(prev.get(KEY)),
        value_as_opt_f64(curr.get(KEY)),
    ) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(p), Some(c)) => {
            let threshold = DEADBAND_POWER_WATTS.max(DEADBAND_POWER_RATIO * p.abs());
            (c - p).abs() >= (threshold - FLOAT_EPSILON)
        }
    }
}

// =============================================================================
/// Extracts `Some(f64)` when `val` is a non-null JSON number, else `None`.
fn value_as_opt_f64(val: Option<&Value>) -> Option<f64> {
    match val {
        None | Some(Value::Null) => None,
        Some(v) => v.as_f64(),
    }
}
