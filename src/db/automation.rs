//! Scoped automation rule persistence (`automation_config`), presets, & Single-Authority API.
//!
//! Enforces the Single-Authority Pattern (`global` / `group` / `device`), rejects standalone
//! device mutations when locked by a software group (`HTTP 409 Conflict`), manages comfort/eco
//! presets, and delegates SQL upsert & `Pre-Heat` mirroring to [`super::automation_sql`].

use std::collections::HashMap;

use rusqlite::params;
use serde_json::{Map, Value, json};

use super::Database;
use super::automation_sql::{
    UpsertParams, default_scope_feature_map, execute_feature_upsert_on, extract_active_flag,
    extract_feature_ts, fetch_scope_feature_cfg_on, harmonize_scope_preheat, load_scope_configs_on,
    order_batch_features, parse_automation_row,
};
use crate::core::domain_constants::{
    automation::{
        automation_defaults::{SCHEDULE_DEFAULT_COMFORT_TEMP, SCHEDULE_DEFAULT_ECO_TEMP},
        automation_identifiers::{
            HEATING_SCHEDULE, KEY_COMFORT_TEMP, KEY_ECO_TEMP, SCOPE_DEVICE, SCOPE_GLOBAL,
            SCOPE_GROUP, VACATION_MODE, WEATHER_SMART_HEATING,
        },
    },
    hardware::special_devices::GROUP_ID_PREFIX,
};
use crate::core::error::{AppError, AppResult};
use crate::core::heating_forecast::RuleSnapshot;
use crate::core::time_util::{build_automation_lww_key, normalize_ain, now_epoch_f64};
use crate::models::automation::{AutomationScope, normalize_feature_name};
use crate::models::group::{format_group_id, parse_group_id};

/// Tolerance when comparing existing slot temperatures against the previous `comfort_temp`.
const PRESET_MATCH_EPSILON: f64 = 1e-4;

/// Tuple of `(prefixed_group_id, merged_scope_rules, global_vacation_rule)`.
pub(super) type ForecastScopeRules = (
    Option<String>,
    HashMap<String, RuleSnapshot>,
    Option<RuleSnapshot>,
);

impl Database {
    /// Validates `(scope_type, scope_id)` and enforces Single Authority on `device` scopes.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on malformed scope or [`AppError::Conflict`] if
    /// `scope_type == "device"` targets a device belonging to a software group.
    pub fn resolve_automation_scope(
        &self,
        scope_type: &str,
        scope_id: &str,
    ) -> AppResult<(String, String)> {
        let parsed = AutomationScope::parse(Some(scope_type), Some(scope_id))?;
        if parsed.scope_type == SCOPE_DEVICE
            && let Some((gid, gname)) = self.get_group_for_device(&parsed.scope_id)?
        {
            return Err(AppError::Conflict(format!(
                "Device '{}' is managed by group '{gname}' ({}). Individual device automations are locked.",
                parsed.scope_id,
                format_group_id(gid)
            )));
        }
        Ok((parsed.scope_type, parsed.scope_id))
    }

    /// Fetches all 7 automation feature configs (`feature -> {is_active, config}`) for a scope.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`], [`AppError::Conflict`], or [`AppError::Database`].
    pub fn get_all_automation_configs(&self, scope_type: &str, scope_id: &str) -> AppResult<Value> {
        let (norm_type, norm_id) = self.resolve_automation_scope(scope_type, scope_id)?;
        self.with_conn(|conn| load_scope_configs_on(conn, &norm_type, &norm_id))
    }

    /// Fetches a single automation feature (`{feature, scope_type, scope_id, is_active, config}`).
    ///
    /// Returns `Ok(None)` when `feature_name` is unrecognized.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`], [`AppError::Conflict`], or [`AppError::Database`].
    pub fn get_automation_config(
        &self,
        feature_name: &str,
        scope_type: &str,
        scope_id: &str,
    ) -> AppResult<Option<Value>> {
        let Some(canonical) = normalize_feature_name(feature_name) else {
            return Ok(None);
        };
        let (norm_type, norm_id) = self.resolve_automation_scope(scope_type, scope_id)?;
        let all = self.with_conn(|conn| load_scope_configs_on(conn, &norm_type, &norm_id))?;
        let entry = all
            .get(canonical)
            .cloned()
            .unwrap_or_else(|| json!({ "is_active": false, "config": {} }));
        Ok(Some(json!({
            "feature": canonical,
            "scope_type": norm_type,
            "scope_id": norm_id,
            "is_active": entry.get("is_active").and_then(Value::as_bool).unwrap_or(false),
            "config": entry.get("config").cloned().unwrap_or_else(|| json!({})),
        })))
    }

    /// Upserts `is_active` and/or `config` for one automation feature with LWW & `Pre-Heat` sync.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`], [`AppError::Conflict`], or [`AppError::Database`].
    pub fn set_automation_config(
        &self,
        feature_name: &str,
        is_active: Option<bool>,
        config: Option<&Value>,
        scope_type: &str,
        scope_id: &str,
        effective_ts: Option<f64>,
    ) -> AppResult<bool> {
        let canonical = normalize_feature_name(feature_name).ok_or_else(|| {
            AppError::Validation(format!("Unknown automation feature: {feature_name}"))
        })?;
        let (norm_type, norm_id) = self.resolve_automation_scope(scope_type, scope_id)?;
        if is_active.is_none() && config.is_none() {
            return Ok(true);
        }
        let eff_ts = effective_ts.unwrap_or_else(now_epoch_f64);
        let lww_key = build_automation_lww_key(&norm_type, &norm_id, canonical);
        if self.is_lww_stale(std::slice::from_ref(&lww_key), eff_ts) {
            return Ok(false);
        }
        let p = UpsertParams {
            feature: canonical,
            scope_type: &norm_type,
            scope_id: &norm_id,
            is_active,
            config,
            eff_ts,
        };
        let mirrored = self.with_tx(|tx| execute_feature_upsert_on(tx, &p))?;
        let mut keys = vec![lww_key];
        if let Some(mk) = mirrored {
            keys.push(mk);
        }
        self.record_lww_mutation(&keys, eff_ts);
        Ok(true)
    }

    /// Updates multiple automation features atomically inside a batch transaction with LWW.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`], [`AppError::Conflict`], or [`AppError::Database`].
    pub fn set_automation_configs_batch(
        &self,
        configs: &Map<String, Value>,
        scope_type: &str,
        scope_id: &str,
        batch_ts: Option<f64>,
    ) -> AppResult<bool> {
        let (norm_type, norm_id) = self.resolve_automation_scope(scope_type, scope_id)?;
        let mut normalized: Vec<(&'static str, &Map<String, Value>)> =
            Vec::with_capacity(configs.len());
        for (raw_name, val) in configs {
            let canon = normalize_feature_name(raw_name).ok_or_else(|| {
                AppError::Validation(format!("Unknown automation feature: {raw_name}"))
            })?;
            let obj = val.as_object().ok_or_else(|| {
                AppError::Validation(format!("Configuration for '{raw_name}' must be an object"))
            })?;
            normalized.push((canon, obj));
        }
        let eff_batch_ts = batch_ts.unwrap_or_else(now_epoch_f64);
        let current = self.with_conn(|conn| load_scope_configs_on(conn, &norm_type, &norm_id))?;
        order_batch_features(&mut normalized, &current);

        let mut any_stale = false;
        let mut applied_mutations: Vec<(String, f64)> = Vec::new();
        self.with_tx(|tx| {
            for &(canon, data) in &normalized {
                let feat_ts = extract_feature_ts(data, eff_batch_ts)?;
                let is_active = extract_active_flag(data);
                let cfg_opt = data.get("config");
                if is_active.is_none() && cfg_opt.is_none() {
                    continue;
                }
                let lww_key = build_automation_lww_key(&norm_type, &norm_id, canon);
                if self.is_lww_stale(std::slice::from_ref(&lww_key), feat_ts) {
                    any_stale = true;
                    continue;
                }
                let p = UpsertParams {
                    feature: canon,
                    scope_type: &norm_type,
                    scope_id: &norm_id,
                    is_active,
                    config: cfg_opt,
                    eff_ts: feat_ts,
                };
                if let Some(mk) = execute_feature_upsert_on(tx, &p)? {
                    applied_mutations.push((mk, feat_ts));
                }
                applied_mutations.push((lww_key, feat_ts));
            }
            Ok(())
        })?;
        for (key, ts) in applied_mutations {
            self.record_lww_mutation(&[key], ts);
        }
        Ok(!any_stale)
    }

    /// Loads all `device`-scoped automation rules mapped by space-normalized AIN.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `automation_config` fails.
    pub fn get_all_device_scope_configs(&self) -> AppResult<HashMap<String, Value>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT scope_id, feature_name, is_active, config_json FROM automation_config
                 WHERE scope_type = ? ORDER BY updated_at ASC, config_id ASC",
            )?;
            let mut by_ain: HashMap<String, Map<String, Value>> = HashMap::new();
            let rows = stmt.query_map(params![SCOPE_DEVICE], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for item in rows {
                let (sid, raw_feat, active_i, raw_json) = item?;
                let Some(canon) = normalize_feature_name(&raw_feat) else {
                    continue;
                };
                let entry = by_ain
                    .entry(normalize_ain(&sid))
                    .or_insert_with(default_scope_feature_map);
                entry.insert(
                    canon.to_owned(),
                    parse_automation_row(canon, active_i, &raw_json),
                );
            }
            Ok(by_ain
                .into_iter()
                .map(|(ain, mut m)| {
                    harmonize_scope_preheat(&mut m);
                    (ain, Value::Object(m))
                })
                .collect())
        })
    }

    /// Resolves the authoritative `(scope_type, scope_id)` for thermostat presets on `target_id`.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `target_id` has an invalid `"group-*"` suffix.
    pub fn resolve_preset_scope(&self, target_id: &str) -> AppResult<(String, String)> {
        let raw = target_id.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case(SCOPE_GLOBAL) {
            return Ok((SCOPE_GLOBAL.to_owned(), SCOPE_GLOBAL.to_owned()));
        }
        if raw.to_ascii_lowercase().starts_with(GROUP_ID_PREFIX) {
            let gid = parse_group_id(raw)
                .ok_or_else(|| AppError::Validation(format!("Invalid group ID '{raw}'")))?;
            return Ok((SCOPE_GROUP.to_owned(), gid.to_string()));
        }
        if let Ok(Some((gid, _))) = self.get_group_for_device(raw) {
            return Ok((SCOPE_GROUP.to_owned(), gid.to_string()));
        }
        Ok((SCOPE_DEVICE.to_owned(), raw.to_owned()))
    }

    /// Resolves authoritative `(comfort_temp, eco_temp)` in `°C` for `target_id`.
    #[must_use]
    pub fn get_thermostat_presets(&self, target_id: &str) -> (f64, f64) {
        let (scope_type, scope_id) = self
            .resolve_preset_scope(target_id)
            .unwrap_or_else(|_| (SCOPE_GLOBAL.to_owned(), SCOPE_GLOBAL.to_owned()));
        let local = self.fetch_scope_feature_cfg(&scope_type, &scope_id, HEATING_SCHEDULE);
        let global = if scope_type == SCOPE_GLOBAL {
            local.clone()
        } else {
            self.fetch_scope_feature_cfg(SCOPE_GLOBAL, SCOPE_GLOBAL, HEATING_SCHEDULE)
        };
        let comfort = local
            .get(KEY_COMFORT_TEMP)
            .or_else(|| global.get(KEY_COMFORT_TEMP))
            .and_then(Value::as_f64)
            .unwrap_or(SCHEDULE_DEFAULT_COMFORT_TEMP);
        let eco = local
            .get(KEY_ECO_TEMP)
            .or_else(|| global.get(KEY_ECO_TEMP))
            .and_then(Value::as_f64)
            .unwrap_or(SCHEDULE_DEFAULT_ECO_TEMP);
        (comfort, eco)
    }

    /// Persists `comfort_temp` and/or `eco_temp` in the authoritative `HeatingSchedule` scope.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid scope or [`AppError::Database`] on SQL error.
    pub fn set_thermostat_presets(
        &self,
        target_id: &str,
        comfort_temp: Option<f64>,
        eco_temp: Option<f64>,
        effective_ts: Option<f64>,
    ) -> AppResult<(f64, f64)> {
        let (scope_type, scope_id) = self.resolve_preset_scope(target_id)?;
        let (cur_comfort, cur_eco) = self.get_thermostat_presets(target_id);
        let mut cfg = self.fetch_scope_feature_cfg(&scope_type, &scope_id, HEATING_SCHEDULE);
        let new_comfort = comfort_temp.unwrap_or(cur_comfort);
        let new_eco = eco_temp.unwrap_or(cur_eco);

        if comfort_temp.is_some() {
            update_matching_slot_temps(&mut cfg, cur_comfort, new_comfort);
        }
        cfg.insert(String::from(KEY_COMFORT_TEMP), json!(new_comfort));
        cfg.insert(String::from(KEY_ECO_TEMP), json!(new_eco));
        let applied = self.set_automation_config(
            HEATING_SCHEDULE,
            None,
            Some(&Value::Object(cfg)),
            &scope_type,
            &scope_id,
            effective_ts,
        )?;
        Ok(if applied {
            (new_comfort, new_eco)
        } else {
            (cur_comfort, cur_eco)
        })
    }

    /// Resolves `(prefixed_group_id, merged_rule_snapshots, global_vacation)` for 5-day forecast.
    #[must_use]
    pub(super) fn resolve_scope_forecast_rules(&self, ain: &str) -> ForecastScopeRules {
        let (scope_type, scope_id) = self
            .resolve_preset_scope(ain)
            .unwrap_or_else(|_| (SCOPE_DEVICE.to_owned(), ain.to_owned()));
        let global_map = self
            .get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)
            .ok()
            .map(value_map_to_snapshots)
            .unwrap_or_default();
        let vac = global_map.get(VACATION_MODE).cloned();
        if scope_type == SCOPE_GLOBAL {
            return (None, global_map, vac);
        }
        let mut rules = self
            .get_all_automation_configs(&scope_type, &scope_id)
            .ok()
            .map(value_map_to_snapshots)
            .unwrap_or_default();
        let norm_ain = normalize_ain(ain);
        for feat in [WEATHER_SMART_HEATING, HEATING_SCHEDULE] {
            let local_on = rules.get(feat).is_some_and(|r| r.is_active);
            let Some(g_rule) = global_map.get(feat).filter(|r| r.is_active) else {
                continue;
            };
            if !local_on && global_affects_ain(&g_rule.config, &norm_ain) {
                rules.insert(feat.to_owned(), g_rule.clone());
            }
        }
        let gid = (scope_type == SCOPE_GROUP)
            .then(|| scope_id.parse::<i64>().ok().map(format_group_id))
            .flatten();
        (gid, rules, vac)
    }

    /// Reads a raw parsed JSON `config` map for `(scope_type, scope_id, feature)`.
    #[must_use]
    pub(super) fn fetch_scope_feature_cfg(
        &self,
        scope_type: &str,
        scope_id: &str,
        feature: &str,
    ) -> Map<String, Value> {
        self.with_conn(|conn| fetch_scope_feature_cfg_on(conn, scope_type, scope_id, feature))
            .unwrap_or_default()
    }
}

// =============================================================================
/// Updates existing schedule blocks whose `"temp"` equaled `old_comfort` to `new_comfort`.
fn update_matching_slot_temps(cfg: &mut Map<String, Value>, old_comfort: f64, new_comfort: f64) {
    let Some(days) = cfg.get_mut("days").and_then(Value::as_object_mut) else {
        return;
    };
    for day_blocks in days.values_mut() {
        let Some(arr) = day_blocks.as_array_mut() else {
            continue;
        };
        for blk in arr {
            let Some(obj) = blk.as_object_mut() else {
                continue;
            };
            if obj
                .get("temp")
                .and_then(Value::as_f64)
                .is_some_and(|t| (t - old_comfort).abs() < PRESET_MATCH_EPSILON)
            {
                obj.insert(String::from("temp"), json!(new_comfort));
            }
        }
    }
}

// =============================================================================
/// Converts a serialized `feature -> {is_active, config}` JSON map into [`RuleSnapshot`]s.
fn value_map_to_snapshots(raw: Value) -> HashMap<String, RuleSnapshot> {
    let Some(obj) = raw.as_object() else {
        return HashMap::new();
    };
    obj.iter()
        .map(|(k, v)| {
            let is_active = v.get("is_active").and_then(Value::as_bool).unwrap_or(false);
            let config = v.get("config").cloned().unwrap_or_else(|| json!({}));
            (k.clone(), RuleSnapshot { is_active, config })
        })
        .collect()
}

// =============================================================================
/// Returns `true` when `affected_devices` is empty/absent or contains `norm_ain`.
fn global_affects_ain(config: &Value, norm_ain: &str) -> bool {
    let Some(arr) = config
        .get("affected_devices")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return true;
    };
    arr.iter()
        .filter_map(Value::as_str)
        .any(|s| normalize_ain(s) == norm_ain)
}
