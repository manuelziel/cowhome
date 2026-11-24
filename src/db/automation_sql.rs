//! SQLite query helpers, `Pre-Heat` mirroring, and legacy key sanitization for `automation.rs`.
//!
//! Keeps [`super::automation`] focused on public `Database` methods while encapsulating
//! low-level `automation_config` upserts, batch ordering, and `AdaptiveStart` synchronization.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::core::domain_constants::automation::automation_identifiers::{
    ADAPTIVE_START, ALL_FEATURES, HEATING_SCHEDULE, HUMIDITY_CONTROL, HUMIDITY_LEGACY_KEYS,
    POWER_SCHEDULE_ALIAS, PREHEAT_LEGACY_ENABLED_KEYS, PREHEAT_LEGACY_STRIP_KEYS, SCOPE_DEVICE,
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::{build_automation_lww_key, normalize_ain, normalize_client_timestamp};
use crate::models::automation::{normalize_config_payload, normalize_feature_name};

// =============================================================================
/// Parameters for a single automation feature upsert inside a SQLite transaction.
pub(super) struct UpsertParams<'a> {
    /// Canonical feature identifier.
    pub feature: &'a str,
    /// Normalized scope type.
    pub scope_type: &'a str,
    /// Normalized scope identifier.
    pub scope_id: &'a str,
    /// Optional explicit activation state.
    pub is_active: Option<bool>,
    /// Optional JSON configuration payload.
    pub config: Option<&'a Value>,
    /// Normalized LWW timestamp (`s`).
    pub eff_ts: f64,
}

// =============================================================================
/// Loads and harmonizes all 7 feature rows for `(norm_type, norm_id)` on `conn`.
pub(super) fn load_scope_configs_on(
    conn: &Connection,
    norm_type: &str,
    norm_id: &str,
) -> AppResult<Value> {
    let mut map = default_scope_feature_map();
    let mut stmt = conn.prepare(
        "SELECT feature_name, is_active, config_json FROM automation_config
         WHERE scope_type = ? AND (scope_id = ? OR replace(scope_id, ' ', '') = ?)
         ORDER BY updated_at ASC, config_id ASC",
    )?;
    let rows = stmt.query_map(params![norm_type, norm_id, normalize_ain(norm_id)], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for item in rows {
        let (raw_feat, active_i, raw_json) = item?;
        if let Some(canon) = normalize_feature_name(&raw_feat) {
            map.insert(
                canon.to_owned(),
                parse_automation_row(canon, active_i, &raw_json),
            );
        }
    }
    harmonize_scope_preheat(&mut map);
    Ok(Value::Object(map))
}

// =============================================================================
/// Initializes the default `{is_active: false, config: {}}` map across `ALL_FEATURES`.
pub(super) fn default_scope_feature_map() -> Map<String, Value> {
    ALL_FEATURES
        .iter()
        .map(|&f| (f.to_owned(), json!({ "is_active": false, "config": {} })))
        .collect()
}

// =============================================================================
/// Parses one `automation_config` row into `{"is_active": bool, "config": {...}}`.
pub(super) fn parse_automation_row(canon: &str, active_int: i64, raw_json: &str) -> Value {
    let mut cfg = serde_json::from_str::<Value>(raw_json)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if canon == HUMIDITY_CONTROL {
        for &k in HUMIDITY_LEGACY_KEYS {
            cfg.remove(k);
        }
    }
    let cfg_enabled = cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    json!({
        "is_active": (active_int != 0) || cfg_enabled,
        "config": cfg,
    })
}

// =============================================================================
/// Harmonizes `HeatingSchedule.config.preheat_enabled` and `AdaptiveStart.is_active`.
pub(super) fn harmonize_scope_preheat(scope_map: &mut Map<String, Value>) {
    let adap_active = scope_map
        .get(ADAPTIVE_START)
        .and_then(|v| v.get("is_active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut preheat_on = adap_active;
    if let Some(s_cfg) = scope_map
        .get_mut(HEATING_SCHEDULE)
        .and_then(|v| v.get_mut("config"))
        .and_then(Value::as_object_mut)
    {
        promote_and_strip_preheat_keys(s_cfg);
        preheat_on = s_cfg
            .get("preheat_enabled")
            .map_or(adap_active, parse_preheat_bool);
        s_cfg.insert(String::from("preheat_enabled"), json!(preheat_on));
    }
    if let Some(adap) = scope_map
        .get_mut(ADAPTIVE_START)
        .and_then(Value::as_object_mut)
    {
        if let Some(a_cfg) = adap.get_mut("config").and_then(Value::as_object_mut) {
            for &k in PREHEAT_LEGACY_STRIP_KEYS {
                a_cfg.remove(k);
            }
        }
        adap.insert(String::from("is_active"), json!(preheat_on));
    }
}

// =============================================================================
/// Promotes legacy preheat boolean aliases into `"preheat_enabled"` and strips legacy keys.
fn promote_and_strip_preheat_keys(cfg: &mut Map<String, Value>) {
    for &alias in PREHEAT_LEGACY_ENABLED_KEYS {
        if let Some(val) = cfg.remove(alias) {
            cfg.entry(String::from("preheat_enabled")).or_insert(val);
        }
    }
    for &legacy in PREHEAT_LEGACY_STRIP_KEYS {
        cfg.remove(legacy);
    }
    if let Some(val) = cfg.get("preheat_enabled").cloned() {
        cfg.insert(
            String::from("preheat_enabled"),
            json!(parse_preheat_bool(&val)),
        );
    }
}

// =============================================================================
/// Parses a `preheat_enabled` JSON scalar (`bool`, `"true"`, `"1"`, `"on"`, or `1`).
pub(super) fn parse_preheat_bool(val: &Value) -> bool {
    match val {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        _ => false,
    }
}

// =============================================================================
/// Executes a single feature upsert + optional `Pre-Heat` mirror on `conn`.
pub(super) fn execute_feature_upsert_on(
    conn: &Connection,
    p: &UpsertParams<'_>,
) -> AppResult<Option<String>> {
    if p.scope_type == SCOPE_DEVICE {
        conn.execute(
            "DELETE FROM automation_config WHERE scope_type = ?
             AND (feature_name = ? OR feature_name = ?)
             AND replace(scope_id, ' ', '') = ? AND (scope_id != ? OR feature_name != ?)",
            params![
                SCOPE_DEVICE,
                p.feature,
                POWER_SCHEDULE_ALIAS,
                normalize_ain(p.scope_id),
                p.scope_id,
                p.feature
            ],
        )?;
    }
    let (sync_act, prepared_cfg) = prepare_config_for_upsert(
        conn,
        p.feature,
        p.scope_type,
        p.scope_id,
        p.is_active,
        p.config,
    )?;
    run_upsert_statement(
        conn,
        p.feature,
        p.scope_type,
        p.scope_id,
        sync_act,
        prepared_cfg.as_ref(),
        p.eff_ts,
    )?;
    mirror_preheat_on(conn, p)
}

/// Prepared `(is_active, config_map)` pair ready for `automation_config` SQL upsert.
type PreparedUpsert = (Option<bool>, Option<Map<String, Value>>);

// =============================================================================
/// Synchronizes `enabled`, merges `HeatingSchedule`/`AdaptiveStart` maps, and strips legacy keys.
fn prepare_config_for_upsert(
    conn: &Connection,
    feature: &str,
    scope_type: &str,
    scope_id: &str,
    is_active: Option<bool>,
    raw_cfg: Option<&Value>,
) -> AppResult<PreparedUpsert> {
    let Some(val) = raw_cfg else {
        return Ok((is_active, None));
    };
    let mut cfg = normalize_config_payload(feature, val)
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut eff_active = is_active.or_else(|| cfg.get("enabled").and_then(Value::as_bool));
    if let Some(act) = eff_active
        && cfg.contains_key("enabled")
    {
        cfg.insert(String::from("enabled"), json!(act));
    }
    if feature == HUMIDITY_CONTROL {
        for &k in HUMIDITY_LEGACY_KEYS {
            cfg.remove(k);
        }
    } else if feature == HEATING_SCHEDULE {
        let mut merged = fetch_scope_feature_cfg_on(conn, scope_type, scope_id, HEATING_SCHEDULE)?;
        merged.extend(cfg);
        promote_and_strip_preheat_keys(&mut merged);
        if let Some(act) = eff_active
            && merged.contains_key("enabled")
        {
            merged.insert(String::from("enabled"), json!(act));
        }
        cfg = merged;
    } else if feature == ADAPTIVE_START {
        let mut merged = fetch_scope_feature_cfg_on(conn, scope_type, scope_id, ADAPTIVE_START)?;
        merged.extend(cfg);
        for &k in PREHEAT_LEGACY_STRIP_KEYS {
            merged.remove(k);
        }
        cfg = merged;
        eff_active = is_active.or(eff_active);
    }
    Ok((eff_active, Some(cfg)))
}

// =============================================================================
/// Runs the `INSERT ... ON CONFLICT DO UPDATE` statement for `automation_config`.
fn run_upsert_statement(
    conn: &Connection,
    feature: &str,
    scope_type: &str,
    scope_id: &str,
    is_active: Option<bool>,
    cfg: Option<&Map<String, Value>>,
    eff_ts: f64,
) -> AppResult<()> {
    match (is_active, cfg) {
        (Some(act), Some(c)) => {
            let s = serde_json::to_string(c)
                .map_err(|e| AppError::Database(format!("Config serialize failed: {e}")))?;
            conn.execute(
                "INSERT INTO automation_config (feature_name, scope_type, scope_id, is_active, config_json, updated_at)
                 VALUES (?, ?, ?, ?, ?, datetime(?, 'unixepoch'))
                 ON CONFLICT(feature_name, scope_type, scope_id) DO UPDATE
                 SET is_active = excluded.is_active, config_json = excluded.config_json, updated_at = excluded.updated_at",
                params![feature, scope_type, scope_id, i64::from(act), s, eff_ts],
            )?;
        }
        (Some(act), None) => {
            conn.execute(
                "INSERT INTO automation_config (feature_name, scope_type, scope_id, is_active, updated_at)
                 VALUES (?, ?, ?, ?, datetime(?, 'unixepoch'))
                 ON CONFLICT(feature_name, scope_type, scope_id) DO UPDATE
                 SET is_active = excluded.is_active,
                     config_json = CASE WHEN json_valid(config_json) AND json_type(config_json, '$.enabled') IS NOT NULL
                                        THEN json_set(config_json, '$.enabled', excluded.is_active)
                                        ELSE config_json END,
                     updated_at = excluded.updated_at",
                params![feature, scope_type, scope_id, i64::from(act), eff_ts],
            )?;
        }
        (None, Some(c)) => {
            let s = serde_json::to_string(c)
                .map_err(|e| AppError::Database(format!("Config serialize failed: {e}")))?;
            conn.execute(
                "INSERT INTO automation_config (feature_name, scope_type, scope_id, config_json, updated_at)
                 VALUES (?, ?, ?, ?, datetime(?, 'unixepoch'))
                 ON CONFLICT(feature_name, scope_type, scope_id) DO UPDATE
                 SET config_json = excluded.config_json, updated_at = excluded.updated_at",
                params![feature, scope_type, scope_id, s, eff_ts],
            )?;
        }
        (None, None) => {}
    }
    Ok(())
}

// =============================================================================
/// Mirrors `Pre-Heat` state between `HeatingSchedule` and `AdaptiveStart` on `conn`.
fn mirror_preheat_on(conn: &Connection, p: &UpsertParams<'_>) -> AppResult<Option<String>> {
    if p.feature == HEATING_SCHEDULE {
        let Some(cfg_obj) = p.config.and_then(Value::as_object) else {
            return Ok(None);
        };
        let has_key = cfg_obj.contains_key("preheat_enabled")
            || PREHEAT_LEGACY_ENABLED_KEYS
                .iter()
                .any(|&k| cfg_obj.contains_key(k));
        if !has_key {
            return Ok(None);
        }
        let sched = fetch_scope_feature_cfg_on(conn, p.scope_type, p.scope_id, HEATING_SCHEDULE)?;
        let preheat_on = sched.get("preheat_enabled").is_some_and(parse_preheat_bool);
        let mut a_cfg = fetch_scope_feature_cfg_on(conn, p.scope_type, p.scope_id, ADAPTIVE_START)?;
        for &k in PREHEAT_LEGACY_STRIP_KEYS {
            a_cfg.remove(k);
        }
        run_upsert_statement(
            conn,
            ADAPTIVE_START,
            p.scope_type,
            p.scope_id,
            Some(preheat_on),
            Some(&a_cfg),
            p.eff_ts,
        )?;
        return Ok(Some(build_automation_lww_key(
            p.scope_type,
            p.scope_id,
            ADAPTIVE_START,
        )));
    }
    if p.feature == ADAPTIVE_START
        && let Some(act) = p.is_active
    {
        let mut s_cfg =
            fetch_scope_feature_cfg_on(conn, p.scope_type, p.scope_id, HEATING_SCHEDULE)?;
        s_cfg.insert(String::from("preheat_enabled"), json!(act));
        promote_and_strip_preheat_keys(&mut s_cfg);
        run_upsert_statement(
            conn,
            HEATING_SCHEDULE,
            p.scope_type,
            p.scope_id,
            None,
            Some(&s_cfg),
            p.eff_ts,
        )?;
        return Ok(Some(build_automation_lww_key(
            p.scope_type,
            p.scope_id,
            HEATING_SCHEDULE,
        )));
    }
    Ok(None)
}

// =============================================================================
/// Fetches `config_json` as a `Map` for `(scope_type, scope_id, feature)` on `conn`.
pub(super) fn fetch_scope_feature_cfg_on(
    conn: &Connection,
    scope_type: &str,
    scope_id: &str,
    feature: &str,
) -> AppResult<Map<String, Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT config_json FROM automation_config
             WHERE feature_name = ? AND scope_type = ?
               AND (scope_id = ? OR replace(scope_id, ' ', '') = ?) LIMIT 1",
            params![feature, scope_type, scope_id, normalize_ain(scope_id)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default())
}

// =============================================================================
/// Orders batch updates so whichever `Pre-Heat` toggle actually changed runs last.
pub(super) fn order_batch_features(
    items: &mut [(&'static str, &Map<String, Value>)],
    current: &Value,
) {
    let cur_on = current
        .get(ADAPTIVE_START)
        .and_then(|v| v.get("is_active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let s_preheat = items
        .iter()
        .find(|&&(f, _)| f == HEATING_SCHEDULE)
        .and_then(|(_, d)| d.get("config")?.get("preheat_enabled"))
        .map(parse_preheat_bool);
    let a_act = items
        .iter()
        .find(|&&(f, _)| f == ADAPTIVE_START)
        .and_then(|(_, d)| extract_active_flag(d));
    let last_feat = if let (Some(sp), Some(aa)) = (s_preheat, a_act) {
        if sp == cur_on && aa != cur_on {
            ADAPTIVE_START
        } else {
            HEATING_SCHEDULE
        }
    } else {
        HEATING_SCHEDULE
    };
    items.sort_by_key(|&(feat, _)| u8::from(feat == last_feat));
}

// =============================================================================
/// Extracts `is_active` / `enabled` / `active` from a batch feature object.
pub(super) fn extract_active_flag(data: &Map<String, Value>) -> Option<bool> {
    for key in ["is_active", "enabled", "active"] {
        if let Some(val) = data.get(key).filter(|v| !v.is_null()) {
            return Some(parse_preheat_bool(val));
        }
    }
    None
}

// =============================================================================
/// Resolves per-feature `client_timestamp` / `timestamp` inside a batch entry.
pub(super) fn extract_feature_ts(data: &Map<String, Value>, fallback: f64) -> AppResult<f64> {
    if let Some(raw) = data
        .get("client_timestamp")
        .or_else(|| data.get("timestamp"))
        .filter(|v| !v.is_null())
    {
        return normalize_client_timestamp(Some(raw), None, None).map(|(ts, _)| ts);
    }
    Ok(fallback)
}
