//! Entity registry, HAN-FUN sub-unit resolution, custom names, LWW sync, & Wi-Fi presence.
//!
//! Provides zero-write cached entity registration (`entities`), space-normalized and `-1`
//! sub-unit entity lookups, Last-Write-Wins (LWW) conflict protection, custom device display
//! names (`device_custom_names`), and smartphone Wi-Fi presence history (`presence_history`).

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};

use super::Database;
use crate::core::domain_constants::{
    analytics::history_defaults::{FLOAT_EPSILON, HEARTBEAT_INTERVAL_SECS},
    hardware::{device_types, fritz_api::SUBUNIT_DEFAULT_SUFFIX},
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::{build_name_lww_key, normalize_ain, now_epoch_f64};

// =============================================================================
/// Resolved row from the `entities` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRow {
    /// Numeric SQLite primary key (`entity_id`).
    pub entity_id: i64,
    /// Canonical FritzBox AIN or virtual identifier (`ROUTER_01`, `WEATHER_01`).
    pub ain: String,
    /// Persisted display name.
    pub name: String,
    /// Standardized device type (`"thermostat"`, `"switch"`, `"sensor"`, etc.).
    pub device_type: String,
}

impl Database {
    /// Returns `true` when `effective_ts` is strictly older than the latest LWW mutation on `keys`.
    #[must_use]
    pub fn is_lww_stale(&self, keys: &[String], effective_ts: f64) -> bool {
        let Ok(guard) = self.inner.lww_timestamps.read() else {
            return false;
        };
        let max_seen = keys
            .iter()
            .filter_map(|k| guard.get(k).copied())
            .fold(0.0_f64, f64::max);
        effective_ts < max_seen
    }

    /// Records `effective_ts` monotonically across all affected LWW `keys`.
    pub fn record_lww_mutation(&self, keys: &[String], effective_ts: f64) {
        if let Ok(mut guard) = self.inner.lww_timestamps.write() {
            for key in keys {
                let entry = guard.entry(key.clone()).or_insert(0.0);
                if effective_ts >= *entry {
                    *entry = effective_ts;
                }
            }
        }
    }

    /// Removes a tracked LWW key after an explicit resource deletion.
    pub fn remove_lww_key(&self, key: &str) {
        if let Ok(mut guard) = self.inner.lww_timestamps.write() {
            guard.remove(key);
        }
    }

    /// Registers an entity (or returns its cached `entity_id` with zero disk writes).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if checking out a connection or executing SQL fails.
    pub fn register_entity(&self, ain: &str, name: &str, device_type: &str) -> AppResult<i64> {
        if let Some(eid) = self.lookup_cached_entity(ain, name, device_type) {
            return Ok(eid);
        }
        self.with_conn(|conn| self.register_entity_on(conn, ain, name, device_type))
    }

    /// Registers an entity on an existing connection/transaction with zero-write caching.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the `SELECT`, `INSERT`, or `UPDATE` fails.
    pub fn register_entity_on(
        &self,
        conn: &Connection,
        ain: &str,
        name: &str,
        device_type: &str,
    ) -> AppResult<i64> {
        if let Some(eid) = self.lookup_cached_entity(ain, name, device_type) {
            return Ok(eid);
        }
        let existing: Option<(i64, String, Option<String>)> = conn
            .query_row(
                "SELECT entity_id, name, device_type FROM entities WHERE ain = ?",
                params![ain],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let eid = if let Some((id, cur_name, cur_type)) = existing {
            if cur_name != name || cur_type.as_deref() != Some(device_type) {
                conn.execute(
                    "UPDATE entities SET name = ?, device_type = ?, updated_at = CURRENT_TIMESTAMP WHERE ain = ?",
                    params![name, device_type, ain],
                )?;
            }
            id
        } else {
            conn.execute(
                "INSERT INTO entities (ain, name, device_type) VALUES (?, ?, ?)",
                params![ain, name, device_type],
            )?;
            conn.last_insert_rowid()
        };
        if let Ok(mut cache) = self.inner.entity_cache.write() {
            cache.insert(
                ain.to_owned(),
                (eid, name.to_owned(), device_type.to_owned()),
            );
        }
        Ok(eid)
    }

    /// Checks the in-memory `entity_cache` for an exact `(ain, name, device_type)` hit.
    fn lookup_cached_entity(&self, ain: &str, name: &str, device_type: &str) -> Option<i64> {
        let guard = self.inner.entity_cache.read().ok()?;
        let (eid, c_name, c_type) = guard.get(ain)?;
        (c_name == name && c_type == device_type).then_some(*eid)
    }

    /// Resolves an entity by exact AIN, space-stripped AIN, or HAN-FUN `-1` sub-unit fallback.
    ///
    /// # Errors
    /// Returns [`AppError::NotFound`] if no matching entity exists, or [`AppError::Database`].
    pub fn resolve_entity(&self, ain: &str) -> AppResult<EntityRow> {
        self.with_conn(|conn| resolve_entity_on(conn, ain))
    }

    /// Sets or updates a custom display name for `ain` with LWW protection and zero-write upsert.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the transaction fails.
    pub fn set_custom_name(
        &self,
        ain: &str,
        custom_name: &str,
        effective_ts: Option<f64>,
    ) -> AppResult<bool> {
        let eff_ts = effective_ts.unwrap_or_else(now_epoch_f64);
        let lww_key = build_name_lww_key(ain);
        if self.is_lww_stale(std::slice::from_ref(&lww_key), eff_ts) {
            return Ok(false);
        }
        let norm = normalize_ain(ain);
        let changed = self.with_tx(|tx| {
            let c1 = tx.execute(
                "DELETE FROM device_custom_names WHERE replace(ain, ' ', '') = ? AND ain != ?",
                params![norm, ain],
            )?;
            let c2 = tx.execute(
                "INSERT INTO device_custom_names (ain, custom_name, updated_at)
                 VALUES (?, ?, datetime(?, 'unixepoch'))
                 ON CONFLICT(ain) DO UPDATE SET custom_name = excluded.custom_name, updated_at = excluded.updated_at
                 WHERE device_custom_names.custom_name IS NOT excluded.custom_name",
                params![ain, custom_name, eff_ts],
            )?;
            let c3 = tx.execute(
                "UPDATE entities SET name = ?, updated_at = datetime(?, 'unixepoch')
                 WHERE (ain = ? OR replace(ain, ' ', '') = ?) AND name IS NOT ?",
                params![custom_name, eff_ts, ain, norm, custom_name],
            )?;
            Ok((c1 + c2 + c3) > 0)
        })?;
        if changed {
            if let Ok(mut cache) = self.inner.entity_cache.write() {
                cache.clear();
            }
        }
        self.record_lww_mutation(&[lww_key], eff_ts);
        Ok(true)
    }

    /// Retrieves a custom display name for `ain` (with space-stripped fallback).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the SQLite query fails.
    pub fn get_custom_name(&self, ain: &str) -> AppResult<Option<String>> {
        let norm = normalize_ain(ain);
        self.with_conn(|conn| {
            if let Some(exact) = conn
                .query_row(
                    "SELECT custom_name FROM device_custom_names WHERE ain = ?",
                    params![ain],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                return Ok(Some(exact));
            }
            conn.query_row(
                "SELECT custom_name FROM device_custom_names WHERE replace(ain, ' ', '') = ?",
                params![norm],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(AppError::from)
        })
    }

    /// Returns all persisted custom device names (`ain -> custom_name`).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `device_custom_names` fails.
    pub fn get_all_custom_names(&self) -> AppResult<HashMap<String, String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT ain, custom_name FROM device_custom_names")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = HashMap::new();
            for pair in rows {
                let (ain, cname) = pair?;
                out.insert(ain, cname);
            }
            Ok(out)
        })
    }

    /// Deletes a custom display name override for `ain`, clearing its LWW timestamp.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the `DELETE` statement fails.
    pub fn delete_custom_name(&self, ain: &str) -> AppResult<bool> {
        let norm = normalize_ain(ain);
        let deleted = self.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM device_custom_names WHERE ain = ? OR replace(ain, ' ', '') = ?",
                params![ain, norm],
            )?;
            Ok(n > 0)
        })?;
        self.remove_lww_key(&build_name_lww_key(ain));
        Ok(deleted)
    }

    /// Records a smartphone Wi-Fi presence ping using change-only deadband, `30 min` heartbeat,
    /// and Pre-Edge Anchor insertion.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying or inserting into `presence_history` fails.
    pub fn insert_presence_ping(
        &self,
        mac_address: &str,
        is_home: bool,
        timestamp: Option<f64>,
    ) -> AppResult<()> {
        let eff_now = timestamp.unwrap_or_else(now_epoch_f64);
        self.with_tx(|tx| self.insert_presence_ping_on(tx, mac_address, is_home, Some(eff_now)))
    }

    /// Records a Wi-Fi presence ping on an existing connection/transaction (batch poll commit).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying or inserting into `presence_history` fails.
    pub fn insert_presence_ping_on(
        &self,
        conn: &Connection,
        mac_address: &str,
        is_home: bool,
        timestamp: Option<f64>,
    ) -> AppResult<()> {
        let norm_mac = mac_address.trim().to_ascii_lowercase();
        let eff_now = timestamp.unwrap_or_else(now_epoch_f64);
        let prev = self.load_presence_baseline_on(conn, &norm_mac)?;
        let mut pre_edge: Option<(bool, f64)> = None;
        if let Some((prev_home, prev_ts)) = prev {
            let changed = prev_home != is_home;
            let hb_due = (eff_now - prev_ts) >= (HEARTBEAT_INTERVAL_SECS - FLOAT_EPSILON);
            if !changed && !hb_due {
                if let Ok(mut caches) = self.inner.recorder_caches.lock() {
                    caches
                        .presence_suppressed
                        .insert(norm_mac, (is_home, eff_now));
                }
                return Ok(());
            }
            if let Ok(mut caches) = self.inner.recorder_caches.lock() {
                if let Some((sup_flag, sup_ts)) = caches.presence_suppressed.remove(&norm_mac) {
                    if changed
                        && (sup_ts as i64) > (prev_ts as i64)
                        && (eff_now as i64) > (sup_ts as i64)
                    {
                        pre_edge = Some((sup_flag, sup_ts));
                    }
                }
            }
        }
        if let Some((sup_flag, sup_ts)) = pre_edge {
            insert_presence_row_on(conn, &norm_mac, sup_flag, sup_ts)?;
        }
        insert_presence_row_on(conn, &norm_mac, is_home, eff_now)?;
        if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            caches.presence_saved.insert(norm_mac, (is_home, eff_now));
        }
        Ok(())
    }

    /// Returns the latest UNIX epoch timestamp (`s`) when `mac_address` was seen online (`is_home = 1`).
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `presence_history` fails.
    pub fn get_last_presence_seen(&self, mac_address: &str) -> AppResult<Option<f64>> {
        let norm_mac = mac_address.trim().to_ascii_lowercase();
        let db_ts = self.with_conn(|conn| {
            conn.query_row(
                "SELECT CAST(strftime('%s', timestamp) AS REAL)
                 FROM presence_history WHERE mac_address = ? AND is_home = 1
                 ORDER BY timestamp DESC, presence_id DESC LIMIT 1",
                params![norm_mac],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()
            .map(|opt| opt.flatten())
            .map_err(AppError::from)
        })?;
        let mut best = db_ts;
        if let Ok(caches) = self.inner.recorder_caches.lock() {
            for map in [&caches.presence_saved, &caches.presence_suppressed] {
                if let Some(&(true, ts)) = map.get(&norm_mac) {
                    best = Some(best.map_or(ts, |cur| cur.max(ts)));
                }
            }
        }
        Ok(best)
    }

    /// Loads the latest persisted presence `(is_home, ts)` for `norm_mac` if not yet cached.
    fn load_presence_baseline_on(
        &self,
        conn: &Connection,
        norm_mac: &str,
    ) -> AppResult<Option<(bool, f64)>> {
        if let Ok(caches) = self.inner.recorder_caches.lock() {
            if let Some(&entry) = caches.presence_saved.get(norm_mac) {
                return Ok(Some(entry));
            }
        }
        let row: Option<(i64, Option<f64>)> = conn
            .query_row(
                "SELECT is_home, CAST(strftime('%s', timestamp) AS REAL)
                 FROM presence_history WHERE mac_address = ?
                 ORDER BY timestamp DESC, presence_id DESC LIMIT 1",
                params![norm_mac],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((flag, Some(ts))) = row else {
            return Ok(None);
        };
        let entry = (flag != 0, ts);
        if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            caches.presence_saved.insert(norm_mac.to_owned(), entry);
        }
        Ok(Some(entry))
    }
}

// =============================================================================
/// Inserts one `presence_history` row at UNIX timestamp `ts`.
fn insert_presence_row_on(conn: &Connection, mac: &str, is_home: bool, ts: f64) -> AppResult<()> {
    conn.execute(
        "INSERT INTO presence_history (mac_address, is_home, timestamp)
         VALUES (?, ?, datetime(?, 'unixepoch'))",
        params![mac, i64::from(is_home), ts],
    )?;
    Ok(())
}

// =============================================================================
/// Resolves an [`EntityRow`] on `conn` with space-stripping and HAN-FUN `-1` fallback.
///
/// # Errors
/// Returns [`AppError::NotFound`] when absent or [`AppError::Database`] on SQL error.
pub(super) fn resolve_entity_on(conn: &Connection, ain: &str) -> AppResult<EntityRow> {
    let mut row = query_single_entity(conn, ain)?;
    let stripped = strip_subunit_suffix(ain);
    if row.is_none() {
        if let Some(ref base_ain) = stripped {
            row = query_single_entity(conn, base_ain)?;
        }
    }
    let is_generic = row
        .as_ref()
        .is_some_and(|r| r.device_type == device_types::GENERIC);
    if (row.is_none() || is_generic) && stripped.is_none() {
        let sub_ain = format!("{}{SUBUNIT_DEFAULT_SUFFIX}", ain.trim());
        if let Some(sub_row) = query_single_entity(conn, &sub_ain)? {
            if row.is_none() || sub_row.device_type != device_types::GENERIC {
                row = Some(sub_row);
            }
        }
    }
    row.ok_or_else(|| AppError::NotFound(format!("Device with AIN '{ain}' not found")))
}

// =============================================================================
/// Queries `entities` for an exact or whitespace-insensitive AIN match.
fn query_single_entity(conn: &Connection, candidate: &str) -> AppResult<Option<EntityRow>> {
    let norm = normalize_ain(candidate);
    conn.query_row(
        "SELECT entity_id, ain, name, COALESCE(device_type, 'generic')
         FROM entities WHERE ain = ? OR replace(ain, ' ', '') = ? LIMIT 1",
        params![candidate, norm],
        |r| {
            Ok(EntityRow {
                entity_id: r.get(0)?,
                ain: r.get(1)?,
                name: r.get(2)?,
                device_type: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(AppError::from)
}

// =============================================================================
/// Extracts the base numeric AIN when `ain` carries a HAN-FUN `-<unit>` suffix (`-1`).
#[must_use]
pub(super) fn strip_subunit_suffix(ain: &str) -> Option<String> {
    let (base, suffix) = ain.trim().rsplit_once('-')?;
    let base_norm = normalize_ain(base);
    if !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
        && !base_norm.is_empty()
        && base_norm.chars().all(|c| c.is_ascii_digit())
    {
        return Some(base.trim().to_owned());
    }
    None
}

// =============================================================================
/// Deduplicates an ordered list of raw AIN strings by their space-stripped form.
#[must_use]
pub(super) fn deduplicate_ains_by_norm(raw_ains: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(raw_ains.len());
    for raw in raw_ains {
        let norm = normalize_ain(&raw);
        if !norm.is_empty() && seen.insert(norm) {
            out.push(raw);
        }
    }
    out
}
