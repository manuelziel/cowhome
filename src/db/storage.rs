//! SQLite history retention pruning (`prune_history`) and storage telemetry (`statvfs`).
//!
//! Serves daily `00:00` retention cleanup (`state_history` & `presence_history`), `POST`
//! maintenance operations, and `GET /api/v1/system/storage` file/sidecar/disk metrics.

use std::ffi::CString;
use std::fs;
use std::path::Path;

use rusqlite::params;
use serde_json::{json, Map, Value};

use super::schema::sidecar_path;
use super::Database;
use crate::core::constants::fhs_paths::STATE_DIR;
use crate::core::domain_constants::analytics::efficiency_defaults::SECONDS_PER_DAY;
use crate::core::error::{AppError, AppResult};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::now_epoch_f64;

/// Binary byte-unit labels used by [`format_bytes_human`].
const STORAGE_UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];

/// Known CowHome user tables counted deterministically without dynamic SQL interpolation.
const USER_TABLES: &[(&str, &str)] = &[
    ("entities", "SELECT COUNT(*) FROM entities"),
    ("state_history", "SELECT COUNT(*) FROM state_history"),
    (
        "automation_config",
        "SELECT COUNT(*) FROM automation_config",
    ),
    (
        "device_custom_names",
        "SELECT COUNT(*) FROM device_custom_names",
    ),
    ("presence_history", "SELECT COUNT(*) FROM presence_history"),
    ("software_groups", "SELECT COUNT(*) FROM software_groups"),
    (
        "software_group_members",
        "SELECT COUNT(*) FROM software_group_members",
    ),
    ("system_settings", "SELECT COUNT(*) FROM system_settings"),
];

/// Tuple of `(table_row_counts, oldest_timestamp, newest_timestamp)`.
type TableStatsSummary = (Map<String, Value>, Option<String>, Option<String>);

impl Database {
    /// Deletes `state_history` and `presence_history` records older than `retention_days`.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `retention_days == 0` or [`AppError::Database`].
    pub fn prune_history(&self, retention_days: u32) -> AppResult<usize> {
        if retention_days == 0 {
            return Err(AppError::Validation(
                "retention_days must be a positive integer".into(),
            ));
        }
        let cutoff_mod = format!("-{retention_days} days");
        let deleted = self.with_tx(|tx| {
            let s = tx.execute(
                "DELETE FROM state_history WHERE timestamp < datetime('now', ?)",
                params![cutoff_mod],
            )?;
            let p = tx.execute(
                "DELETE FROM presence_history WHERE timestamp < datetime('now', ?)",
                params![cutoff_mod],
            )?;
            Ok(s + p)
        })?;
        let cutoff_ts = now_epoch_f64() - f64::from(retention_days) * SECONDS_PER_DAY;
        if let Ok(mut caches) = self.inner.recorder_caches.lock() {
            caches.evict_presence_older_than(cutoff_ts);
        }
        self.inner.logger.info(&format!(
            "Pruned {deleted} history records older than {retention_days} days."
        ));
        Ok(deleted)
    }

    /// Collects SQLite file sizes (`.db`, `-wal`, `-shm`), per-table row counts, and `statvfs` disk stats.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying table metadata fails.
    pub fn get_storage_stats(&self) -> AppResult<Value> {
        let db_path = self.db_path();
        let (db_sz, wal_sz, shm_sz) = read_sidecar_sizes(db_path);
        let total_sz = db_sz + wal_sz + shm_sz;
        let (row_counts, oldest, newest) = self.query_table_stats()?;
        let hist_cnt = row_counts
            .get("state_history")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let ent_cnt = row_counts
            .get("entities")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let mount_dir = resolve_mount_dir(db_path);

        Ok(json!({
            "database": {
                "path": db_path.display().to_string(),
                "db_size_bytes": db_sz,
                "db_size_human": format_bytes_human(db_sz),
                "wal_size_bytes": wal_sz,
                "wal_size_human": format_bytes_human(wal_sz),
                "shm_size_bytes": shm_sz,
                "shm_size_human": format_bytes_human(shm_sz),
                "total_size_bytes": total_sz,
                "total_size_human": format_bytes_human(total_sz),
                "history_records_count": hist_cnt,
                "entities_count": ent_cnt,
                "row_counts": row_counts,
                "oldest_record": oldest,
                "newest_record": newest,
            },
            "disk": query_disk_space_stats(&mount_dir),
        }))
    }

    /// Queries per-table row counts and `MIN`/`MAX` timestamps in `state_history`.
    fn query_table_stats(&self) -> AppResult<TableStatsSummary> {
        self.with_conn(|conn| {
            let mut counts = Map::new();
            for &(name, sql) in USER_TABLES {
                let cnt: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0);
                counts.insert(name.to_owned(), json!(cnt));
            }
            let (oldest, newest): (Option<String>, Option<String>) = conn.query_row(
                "SELECT MIN(timestamp), MAX(timestamp) FROM state_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((counts, oldest, newest))
        })
    }
}

// =============================================================================
/// Reads `(db_bytes, wal_bytes, shm_bytes)` on disk (`0` for `:memory:` or absent files).
fn read_sidecar_sizes(db_path: &Path) -> (u64, u64, u64) {
    if db_path.as_os_str() == ":memory:" {
        return (0, 0, 0);
    }
    let sz = |p: &Path| fs::metadata(p).map_or(0, |m| m.len());
    (
        sz(db_path),
        sz(&sidecar_path(db_path, "-wal")),
        sz(&sidecar_path(db_path, "-shm")),
    )
}

// =============================================================================
/// Resolves a valid existing directory to pass into `statvfs`.
fn resolve_mount_dir(db_path: &Path) -> String {
    if db_path.as_os_str() != ":memory:" {
        if let Some(parent) = db_path.parent().filter(|p| p.exists()) {
            return parent.display().to_string();
        }
    }
    if Path::new(STATE_DIR).exists() {
        STATE_DIR.to_owned()
    } else {
        String::from(".")
    }
}

// =============================================================================
/// Queries POSIX `statvfs` filesystem capacity and usage percentage for `mount_dir`.
fn query_disk_space_stats(mount_dir: &str) -> Value {
    let Ok(c_path) = CString::new(mount_dir) else {
        return empty_disk_stats(mount_dir);
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is stack-allocated.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return empty_disk_stats(mount_dir);
    }
    let frsize = stat.f_frsize as u64;
    let free_bytes = (stat.f_bavail as u64).saturating_mul(frsize);
    let total_bytes = (stat.f_blocks as u64).saturating_mul(frsize);
    let bfree_bytes = (stat.f_bfree as u64).saturating_mul(frsize);
    let used_bytes = total_bytes.saturating_sub(bfree_bytes);
    let used_pct = if total_bytes > 0 {
        round_decimals((used_bytes as f64 / total_bytes as f64) * 100.0, 1)
    } else {
        0.0
    };
    json!({
        "mount_point": mount_dir,
        "free_bytes": free_bytes,
        "free_human": format_bytes_human(free_bytes),
        "total_bytes": total_bytes,
        "total_human": format_bytes_human(total_bytes),
        "used_percent": used_pct,
    })
}

// =============================================================================
/// Returns a zeroed disk-telemetry fallback object when `statvfs` is unavailable.
fn empty_disk_stats(mount_dir: &str) -> Value {
    json!({
        "mount_point": mount_dir,
        "free_bytes": 0,
        "free_human": "0.00 B",
        "total_bytes": 0,
        "total_human": "0.00 B",
        "used_percent": 0.0,
    })
}

// =============================================================================
/// Formats a byte count into a human-readable IEC-scaled string (`"14.06 MB"`).
#[must_use]
pub fn format_bytes_human(size_bytes: u64) -> String {
    let mut val = size_bytes as f64;
    for (idx, &unit) in STORAGE_UNITS.iter().enumerate() {
        if val < 1024.0 || idx + 1 == STORAGE_UNITS.len() {
            return format!("{val:.2} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.2} B")
}
