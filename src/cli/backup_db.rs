//! SQLite domain entity backup and restore (`02_linux/cowhome` CLI parity).
//!
//! Exports and restores virtual `software_groups`, `automation_configs`, standalone
//! `device_automation_configs`, `custom_names`, and `system_settings` (`EnWG § 40`
//! electricity tariff & monthly base fee) with Single-Authority & `--overwrite` guards.

use std::collections::HashMap;

use chrono::Local;
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::core::config::AppConfig;
use crate::core::constants::app_meta::{SOFTWARE_NAME, VERSION};
use crate::core::domain_constants::{
    analytics::energy_defaults::{
        BACKUP_BASE_FEE_KEYS as BASE_FEE_KEYS, BACKUP_PRICE_KEYS as PRICE_KEYS,
        DEFAULT_BASE_FEE_EUR_PER_MONTH, DEFAULT_PRICE_EUR_PER_KWH, SETTING_KEY_BASE_FEE,
        SETTING_KEY_PRICE,
    },
    automation::automation_identifiers::{
        HEATING_SCHEDULE, SCOPE_DEVICE, SCOPE_GLOBAL, SCOPE_GROUP,
    },
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::db::Database;
use crate::models::automation::normalize_feature_name;

/// Canonical top-level keys identifying a CowHome SQLite backup document.
const DB_BACKUP_KEYS: &[&str] = &[
    "export_info",
    "automation_configs",
    "device_automation_configs",
    "software_groups",
    "custom_names",
    "system_settings",
];

// =============================================================================
/// Returns `true` if `val` contains at least one CowHome SQLite backup section key.
#[must_use]
pub fn is_db_backup_payload(val: &Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    DB_BACKUP_KEYS.iter().any(|k| obj.contains_key(*k))
}

// =============================================================================
/// Builds the complete CowHome backup JSON structure from SQLite (or defaults if absent).
///
/// # Errors
/// Returns [`AppError`] if an existing SQLite database cannot be opened or queried.
pub fn build_db_backup_value(config: &AppConfig, logger: &Logger) -> AppResult<Value> {
    let exported_at = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let db_path = config.resolved_database_path();
    if !db_path.exists() {
        return Ok(empty_backup_document(&exported_at));
    }
    let db = Database::open(&db_path, &config.weather.timezone, logger.clone())?;
    let global_auto = db.get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)?;
    let dev_auto = serialize_sorted_value_map(db.get_all_device_scope_configs()?);
    let groups = db.get_all_software_groups()?;
    let names = serialize_sorted_str_map(db.get_all_custom_names()?);
    let settings = json!({
        SETTING_KEY_PRICE: db.get_electricity_price(),
        SETTING_KEY_BASE_FEE: db.get_electricity_base_fee(),
    });
    db.close();

    Ok(json!({
        "export_info": {
            "software": SOFTWARE_NAME,
            "version": VERSION,
            "exported_at": exported_at,
        },
        "automation_configs": global_auto,
        "device_automation_configs": dev_auto,
        "software_groups": groups,
        "custom_names": names,
        "system_settings": settings,
    }))
}

// =============================================================================
/// Returns an empty baseline backup document when `cowhome.db` has not been created yet.
fn empty_backup_document(exported_at: &str) -> Value {
    json!({
        "export_info": {
            "software": SOFTWARE_NAME,
            "version": VERSION,
            "exported_at": exported_at,
        },
        "automation_configs": {},
        "device_automation_configs": {},
        "software_groups": [],
        "custom_names": {},
        "system_settings": {
            SETTING_KEY_PRICE: DEFAULT_PRICE_EUR_PER_KWH,
            SETTING_KEY_BASE_FEE: DEFAULT_BASE_FEE_EUR_PER_MONTH,
        },
    })
}

// =============================================================================
/// Converts a `HashMap<String, Value>` into a deterministically key-ordered JSON object.
fn serialize_sorted_value_map(map: HashMap<String, Value>) -> Value {
    let mut pairs: Vec<(String, Value)> = map.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(pairs.into_iter().collect())
}

// =============================================================================
/// Converts a `HashMap<String, String>` into a deterministically key-ordered JSON object.
fn serialize_sorted_str_map(map: HashMap<String, String>) -> Value {
    let mut pairs: Vec<(String, Value)> = map
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(pairs.into_iter().collect())
}

// =============================================================================
/// Restores CowHome groups, automations, custom names, and tariffs into SQLite.
///
/// # Errors
/// Returns [`AppError`] on invalid payload sections or SQLite write failures.
pub fn import_db_backup_value(
    data: &Value,
    overwrite: bool,
    config: &AppConfig,
    logger: &Logger,
) -> AppResult<()> {
    let obj = data
        .as_object()
        .ok_or_else(|| AppError::Validation("Backup root must be a mapping/object".into()))?;
    let db_path = config.resolved_database_path();
    let db = Database::open(&db_path, &config.weather.timezone, logger.clone())?;

    let result = (|| -> AppResult<()> {
        if let Some(groups) = obj.get("software_groups") {
            import_software_groups(&db, groups, overwrite)?;
        }
        if let Some(dev_autos) = obj.get("device_automation_configs") {
            import_device_automations(&db, dev_autos, overwrite, logger)?;
        }
        if let Some(glob_autos) = obj.get("automation_configs") {
            import_scope_automations(&db, glob_autos, overwrite, SCOPE_GLOBAL, SCOPE_GLOBAL)?;
        }
        if let Some(names) = obj.get("custom_names") {
            import_custom_names(&db, names, overwrite)?;
        }
        if let Some(settings) = obj.get("system_settings") {
            import_system_settings(&db, settings, overwrite)?;
        }
        Ok(())
    })();

    db.close();
    result
}

// =============================================================================
/// Imports `software_groups`, member lists, and group-scoped automation configurations.
///
/// # Errors
/// Returns [`AppError`] if `raw_groups` is not a list or group SQL operations fail.
fn import_software_groups(db: &Database, raw_groups: &Value, overwrite: bool) -> AppResult<()> {
    let list = raw_groups.as_array().ok_or_else(|| {
        AppError::Validation("Invalid 'software_groups' section (expected list)".into())
    })?;
    let mut existing: HashMap<String, String> = db
        .get_all_software_groups()?
        .into_iter()
        .filter_map(|g| {
            Some((
                g.get("name")?.as_str()?.to_owned(),
                g.get("id")?.as_str()?.to_owned(),
            ))
        })
        .collect();

    for group in list {
        let Some(name) = group.get("name").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let members = extract_string_list(group.get("members"));
        let gid = upsert_imported_group(db, name, &members, &mut existing, overwrite)?;
        if let Some(auto) = group.get("automation").filter(|v| v.is_object()) {
            import_scope_automations(db, auto, overwrite, SCOPE_GROUP, &gid)?;
        }
    }
    Ok(())
}

// =============================================================================
/// Creates or updates a single software group by `name`, returning its canonical numeric ID.
///
/// # Errors
/// Returns [`AppError`] on member exclusivity conflict or SQLite failure.
fn upsert_imported_group(
    db: &Database,
    name: &str,
    members: &[String],
    existing: &mut HashMap<String, String>,
    overwrite: bool,
) -> AppResult<String> {
    if let Some(gid) = existing.get(name).cloned() {
        if overwrite {
            db.update_software_group(&gid, Some(name), Some(members), None)?;
        }
        return Ok(gid);
    }
    let created = db.create_software_group(name, members, None)?;
    let gid = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("1")
        .to_owned();
    existing.insert(name.to_owned(), gid.clone());
    Ok(gid)
}

// =============================================================================
/// Extracts a `Vec<String>` from an optional JSON array value.
fn extract_string_list(val: Option<&Value>) -> Vec<String> {
    val.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// =============================================================================
/// Imports standalone `device_automation_configs`, skipping grouped devices (`Conflict`).
///
/// # Errors
/// Returns [`AppError`] if `raw_dev_autos` is not a mapping or SQL persistence fails.
fn import_device_automations(
    db: &Database,
    raw_dev_autos: &Value,
    overwrite: bool,
    logger: &Logger,
) -> AppResult<()> {
    let map = raw_dev_autos.as_object().ok_or_else(|| {
        AppError::Validation(
            "Invalid 'device_automation_configs' section (expected mapping)".into(),
        )
    })?;
    for (ain, features) in map {
        let clean_ain = ain.trim();
        if clean_ain.is_empty() || !features.is_object() {
            continue;
        }
        match import_scope_automations(db, features, overwrite, SCOPE_DEVICE, clean_ain) {
            Ok(()) => {}
            Err(AppError::Conflict(_)) => {
                logger.info(&format!(
                    "Skipping device automations for grouped device '{clean_ain}'"
                ));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

// =============================================================================
/// Imports automation features for `(scope_type, scope_id)`, ordering `heating_schedule` last.
///
/// # Errors
/// Returns [`AppError`] on invalid scope, Single-Authority conflict, or SQLite error.
fn import_scope_automations(
    db: &Database,
    raw_autos: &Value,
    overwrite: bool,
    scope_type: &str,
    scope_id: &str,
) -> AppResult<()> {
    let map = raw_autos.as_object().ok_or_else(|| {
        AppError::Validation("Invalid 'automation_configs' section (expected mapping)".into())
    })?;
    let existing = db.get_all_automation_configs(scope_type, scope_id)?;
    let mut ordered: Vec<(&str, &Value)> = map
        .iter()
        .filter_map(|(k, v)| normalize_feature_name(k).map(|canon| (canon, v)))
        .collect();
    ordered.sort_by_key(|&(feat, _)| u8::from(feat == HEATING_SCHEDULE));

    for (feat, payload) in ordered {
        let Some(p_obj) = payload.as_object() else {
            continue;
        };
        if !overwrite && scope_feature_already_configured(&existing, feat) {
            continue;
        }
        let is_act = p_obj.get("is_active").and_then(Value::as_bool);
        let cfg = p_obj.get("config").filter(|v| v.is_object());
        db.set_automation_config(feat, is_act, cfg, scope_type, scope_id, None)?;
    }
    Ok(())
}

// =============================================================================
/// Checks whether `feat` already has active rules or non-default parameters in `existing`.
fn scope_feature_already_configured(existing: &Value, feat: &str) -> bool {
    let Some(curr) = existing.get(feat).and_then(Value::as_object) else {
        return false;
    };
    if curr.get("is_active").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let Some(cfg_obj) = curr.get("config").and_then(Value::as_object) else {
        return false;
    };
    cfg_obj
        .iter()
        .any(|(k, v)| k != "preheat_enabled" || v.as_bool() == Some(true))
}

// =============================================================================
/// Imports custom device display names (`custom_names`), honoring `overwrite`.
///
/// # Errors
/// Returns [`AppError`] if `raw_names` is not a mapping or SQLite updates fail.
fn import_custom_names(db: &Database, raw_names: &Value, overwrite: bool) -> AppResult<()> {
    let map = raw_names.as_object().ok_or_else(|| {
        AppError::Validation("Invalid 'custom_names' section (expected mapping)".into())
    })?;
    let existing = db.get_all_custom_names()?;
    for (ain, val) in map {
        let clean_ain = ain.trim();
        let Some(clean_name) = val.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if clean_ain.is_empty() || (!overwrite && existing.contains_key(clean_ain)) {
            continue;
        }
        db.set_custom_name(clean_ain, clean_name, None)?;
    }
    Ok(())
}

// =============================================================================
/// Imports electricity tariff and monthly base fee into `system_settings`.
///
/// # Errors
/// Returns [`AppError`] on invalid `system_settings` mapping, tariff bounds, or SQLite error.
fn import_system_settings(db: &Database, raw_settings: &Value, overwrite: bool) -> AppResult<()> {
    let map = raw_settings.as_object().ok_or_else(|| {
        AppError::Validation("Invalid 'system_settings' section (expected mapping)".into())
    })?;
    if let Some(price_val) = first_present_value(map, PRICE_KEYS)
        && (overwrite || !setting_row_exists(db, SETTING_KEY_PRICE)?)
    {
        db.set_electricity_price(price_val, None)?;
    }
    if let Some(fee_val) = first_present_value(map, BASE_FEE_KEYS)
        && (overwrite || !setting_row_exists(db, SETTING_KEY_BASE_FEE)?)
    {
        db.set_electricity_base_fee(fee_val, None)?;
    }
    Ok(())
}

// =============================================================================
/// Returns the first non-`null` value matching any key in `candidates`.
fn first_present_value<'a>(map: &'a Map<String, Value>, candidates: &[&str]) -> Option<&'a Value> {
    candidates
        .iter()
        .find_map(|&k| map.get(k).filter(|v| !v.is_null()))
}

// =============================================================================
/// Returns `true` if `setting_key` already exists in `system_settings`.
///
/// # Errors
/// Returns [`AppError::Database`] if querying `system_settings` fails.
fn setting_row_exists(db: &Database, setting_key: &str) -> AppResult<bool> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT 1 FROM system_settings WHERE setting_key = ? LIMIT 1",
            params![setting_key],
            |_| Ok(true),
        )
        .optional()
        .map(|opt| opt.unwrap_or(false))
        .map_err(AppError::from)
    })
}
