//! SQLite baseline DDL (`PRAGMA user_version = 1`), indexes, and LWW timestamp seeding.
//!
//! Creates all 8 CowHome tables and 8 performance indexes on startup, verifies schema
//! compatibility, seeds in-memory LWW timestamps from `updated_at`, and hands off file
//! ownership of `.db`, `-wal`, and `-shm` to `cowhome:cowhome` when executed as `root`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::core::constants::{
    database_defaults::DB_SCHEMA_VERSION,
    service_defaults::{SERVICE_GROUP, SERVICE_USER},
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::security::apply_ownership_if_root;
use crate::core::time_util::{
    build_automation_lww_key, build_group_lww_key, build_name_lww_key, build_setting_lww_key,
};

/// Baseline table definitions matching CowHome schema `v1`.
const SCHEMA_TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS entities (
        entity_id INTEGER PRIMARY KEY AUTOINCREMENT,
        ain TEXT UNIQUE NOT NULL,
        name TEXT NOT NULL,
        device_type TEXT,
        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
    )",
    "CREATE TABLE IF NOT EXISTS state_history (
        state_id INTEGER PRIMARY KEY AUTOINCREMENT,
        entity_id INTEGER NOT NULL,
        state_json TEXT NOT NULL,
        timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (entity_id) REFERENCES entities(entity_id) ON DELETE CASCADE
    )",
    "CREATE TABLE IF NOT EXISTS automation_config (
        config_id INTEGER PRIMARY KEY AUTOINCREMENT,
        feature_name TEXT NOT NULL,
        scope_type TEXT NOT NULL DEFAULT 'global',
        scope_id TEXT NOT NULL DEFAULT 'global',
        is_active INTEGER NOT NULL DEFAULT 0,
        config_json TEXT NOT NULL DEFAULT '{}',
        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
        UNIQUE(feature_name, scope_type, scope_id)
    )",
    "CREATE TABLE IF NOT EXISTS device_custom_names (
        ain TEXT PRIMARY KEY,
        custom_name TEXT NOT NULL,
        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
    )",
    "CREATE TABLE IF NOT EXISTS presence_history (
        presence_id INTEGER PRIMARY KEY AUTOINCREMENT,
        mac_address TEXT NOT NULL,
        is_home INTEGER NOT NULL,
        timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
    )",
    "CREATE TABLE IF NOT EXISTS software_groups (
        group_id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
    )",
    "CREATE TABLE IF NOT EXISTS software_group_members (
        group_id INTEGER NOT NULL,
        ain TEXT NOT NULL UNIQUE,
        PRIMARY KEY (group_id, ain),
        FOREIGN KEY (group_id) REFERENCES software_groups(group_id) ON DELETE CASCADE
    )",
    "CREATE TABLE IF NOT EXISTS system_settings (
        setting_key TEXT PRIMARY KEY,
        setting_value TEXT NOT NULL,
        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
    )",
];

/// Baseline B-tree indexes accelerating time-series and scope queries.
const SCHEMA_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_entities_ain ON entities(ain)",
    "CREATE INDEX IF NOT EXISTS idx_state_history_entity ON state_history(entity_id, timestamp DESC)",
    "CREATE INDEX IF NOT EXISTS idx_state_history_timestamp ON state_history(timestamp)",
    "CREATE INDEX IF NOT EXISTS idx_custom_names_ain ON device_custom_names(ain)",
    "CREATE INDEX IF NOT EXISTS idx_presence_history_mac ON presence_history(mac_address, timestamp DESC)",
    "CREATE INDEX IF NOT EXISTS idx_group_members_ain ON software_group_members(ain)",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_group_members_unique_ain ON software_group_members(ain)",
    "CREATE INDEX IF NOT EXISTS idx_automation_config_scope ON automation_config(scope_type, scope_id)",
];

// =============================================================================
/// Initializes tables, indexes, and `PRAGMA user_version = 1`, verifying compatibility.
///
/// # Errors
/// Returns [`AppError::Database`] if DDL execution fails or `user_version > DB_SCHEMA_VERSION`.
pub(super) fn initialize_and_verify_schema(conn: &Connection, logger: &Logger) -> AppResult<()> {
    let current_ver: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current_ver > DB_SCHEMA_VERSION {
        return Err(AppError::Database(format!(
            "Schema mismatch: database is v{current_ver}, application expects v{DB_SCHEMA_VERSION}"
        )));
    }
    if current_ver == 0 {
        logger.info("Initializing baseline SQLite schema (v1)...");
    }
    for ddl in SCHEMA_TABLES {
        conn.execute_batch(ddl)?;
    }
    for idx in SCHEMA_INDEXES {
        conn.execute_batch(idx)?;
    }
    if current_ver < DB_SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", DB_SCHEMA_VERSION)?;
    }
    Ok(())
}

// =============================================================================
/// Hands off ownership of `db_path`, `-wal`, and `-shm` to `cowhome:cowhome` when root.
pub(super) fn ensure_database_ownership(db_path: &Path, logger: &Logger) {
    if db_path.as_os_str() == ":memory:" {
        return;
    }
    for candidate in [
        db_path.to_path_buf(),
        sidecar_path(db_path, "-wal"),
        sidecar_path(db_path, "-shm"),
    ] {
        if candidate.exists()
            && let Err(err) = apply_ownership_if_root(&candidate, SERVICE_USER, SERVICE_GROUP)
        {
            logger.debug(&format!(
                "Skipped DB ownership handoff on '{}': {err}",
                candidate.display()
            ));
        }
    }
}

// =============================================================================
/// Appends `suffix` (`"-wal"` or `"-shm"`) onto `base`.
pub(super) fn sidecar_path(base: &Path, suffix: &str) -> PathBuf {
    let mut raw = base.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

// =============================================================================
/// Seeds the in-memory LWW timestamp map from `updated_at` columns across mutable tables.
pub(super) fn seed_lww_timestamps(conn: &Connection) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    seed_automation_lww(conn, &mut map);
    seed_groups_and_names_lww(conn, &mut map);
    map
}

// =============================================================================
/// Loads persisted LWW timestamps from `automation_config`.
fn seed_automation_lww(conn: &Connection, out: &mut HashMap<String, f64>) {
    let sql = "SELECT feature_name, scope_type, scope_id, CAST(strftime('%s', updated_at) AS REAL)
               FROM automation_config WHERE updated_at IS NOT NULL";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
        ))
    }) else {
        return;
    };
    for (feat, stype, sid, ts) in rows.flatten() {
        if ts > 0.0 {
            out.insert(build_automation_lww_key(&stype, &sid, &feat), ts);
        }
    }
}

// =============================================================================
/// Loads persisted LWW timestamps from `software_groups`, `device_custom_names`, & `system_settings`.
fn seed_groups_and_names_lww(conn: &Connection, out: &mut HashMap<String, f64>) {
    if let Ok(mut stmt) = conn.prepare(
        "SELECT group_id, CAST(strftime('%s', updated_at) AS REAL) FROM software_groups WHERE updated_at IS NOT NULL",
    )
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0)))) {
            for (gid, ts) in rows.flatten().filter(|&(_, ts)| ts > 0.0) {
                out.insert(build_group_lww_key(gid), ts);
            }
        }
    for (sql, builder) in [
        (
            "SELECT ain, CAST(strftime('%s', updated_at) AS REAL) FROM device_custom_names WHERE updated_at IS NOT NULL",
            build_name_lww_key as fn(&str) -> String,
        ),
        (
            "SELECT setting_key, CAST(strftime('%s', updated_at) AS REAL) FROM system_settings WHERE updated_at IS NOT NULL",
            build_setting_lww_key as fn(&str) -> String,
        ),
    ] {
        if let Ok(mut stmt) = conn.prepare(sql)
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                ))
            })
        {
            for (key, ts) in rows.flatten().filter(|&(_, ts)| ts > 0.0) {
                out.insert(builder(&key), ts);
            }
        }
    }
}
