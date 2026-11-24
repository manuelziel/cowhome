//! Pure configuration binding, global fallback merging, & schedule slot resolvers.
//!
//! Separated from [`super::scopes`] so pure JSON configuration transformations and
//! Stateful `Database` / `StateRegistry` enrichment each stay concise (`<= 380` lines).

use std::collections::HashSet;

use chrono::DateTime;
use chrono_tz::Tz;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::schedule::HeatingScheduleRule;
use crate::core::domain_constants::{
    automation::automation_identifiers::{
        ADAPTIVE_START, APPARENT_TEMPERATURE, HEATING_SCHEDULE, HUMIDITY_CONTROL, KEY_COMFORT_TEMP,
        KEY_ECO_TEMP, PREHEAT_LEGACY_MERGE_KEYS as PREHEAT_LEGACY_KEYS, PRESENCE_PROFILING,
        SCOPE_DEVICE, SCOPE_GLOBAL, WEATHER_SMART_HEATING,
    },
    hardware::device_types,
};
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::models::automation::{HeatingScheduleConfig, normalize_config_payload};
use crate::models::registry::StateRegistry;

/// Feature configuration map (`feature_name -> {"is_active": bool, "config": {...}}`).
pub type ScopeConfigMap = Map<String, Value>;

// =============================================================================
/// Extracts `(is_active, typed_config)` for `feature` from `configs`.
#[must_use]
pub fn parse_feature_cfg<T>(configs: &ScopeConfigMap, feature: &str) -> (bool, T)
where
    T: DeserializeOwned + Default,
{
    let Some(entry) = configs.get(feature) else {
        return (false, T::default());
    };
    let is_active = entry
        .get("is_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw_cfg = entry.get("config").cloned().unwrap_or_else(|| json!({}));
    let norm = normalize_config_payload(feature, &raw_cfg);
    let parsed = serde_json::from_value(norm).unwrap_or_default();
    (is_active, parsed)
}

// =============================================================================
/// Binds group or device target AINs and shared room sensor into `configs`.
#[must_use]
pub fn bind_configs_to_targets(
    configs: &ScopeConfigMap,
    targets: &[String],
    group_sensor: &str,
) -> ScopeConfigMap {
    let extract_s = |feat: &str| {
        configs
            .get(feat)
            .and_then(|e| e.get("config"))
            .and_then(|c| c.get("sensor_ain"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let shared_sensor = extract_s(APPARENT_TEMPERATURE)
        .or_else(|| extract_s(HUMIDITY_CONTROL))
        .unwrap_or_else(|| group_sensor.to_owned());

    let mut bound = ScopeConfigMap::new();
    for (feat, data) in configs {
        let mut cfg = data
            .get("config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !shared_sensor.is_empty()
            && matches!(feat.as_str(), APPARENT_TEMPERATURE | HUMIDITY_CONTROL)
            && cfg
                .get("sensor_ain")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
        {
            cfg.insert(String::from("sensor_ain"), json!(shared_sensor));
        }
        bind_single_feature(feat, &mut cfg, targets);
        let active = data
            .get("is_active")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        bound.insert(feat.clone(), json!({ "is_active": active, "config": cfg }));
    }
    if !shared_sensor.is_empty() && !bound.contains_key(HUMIDITY_CONTROL) {
        bound.insert(
            HUMIDITY_CONTROL.to_owned(),
            json!({
                "is_active": false,
                "config": { "sensor_ain": shared_sensor, "actor_ain": targets }
            }),
        );
    }
    let preheat_on = bound
        .get(HEATING_SCHEDULE)
        .and_then(|e| e.get("config"))
        .is_some_and(|c| {
            PREHEAT_LEGACY_KEYS
                .iter()
                .any(|k| c.get(*k).and_then(Value::as_bool).unwrap_or(false))
        });
    if preheat_on {
        let mut adp_cfg = bound
            .get(ADAPTIVE_START)
            .and_then(|e| e.get("config"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        bind_single_feature(ADAPTIVE_START, &mut adp_cfg, targets);
        bound.insert(
            ADAPTIVE_START.to_owned(),
            json!({ "is_active": true, "config": adp_cfg }),
        );
    }
    bound
}

// =============================================================================
/// Merges active `global` fallback rules into `bound` when not locally active.
#[must_use]
pub fn merge_global_fallbacks(
    mut bound: ScopeConfigMap,
    global_configs: &ScopeConfigMap,
    targets: &[String],
) -> ScopeConfigMap {
    let local_sched = bound
        .get(HEATING_SCHEDULE)
        .and_then(|e| e.get("is_active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !local_sched {
        bound.insert(
            ADAPTIVE_START.to_owned(),
            json!({ "is_active": false, "config": {} }),
        );
    }
    for feat in [
        WEATHER_SMART_HEATING,
        PRESENCE_PROFILING,
        HEATING_SCHEDULE,
        ADAPTIVE_START,
        HUMIDITY_CONTROL,
        APPARENT_TEMPERATURE,
    ] {
        let local_on = bound
            .get(feat)
            .and_then(|e| e.get("is_active"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if local_on || (feat == ADAPTIVE_START && local_sched) {
            continue;
        }
        let Some(g_data) = global_configs.get(feat) else {
            continue;
        };
        if !g_data
            .get("is_active")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(mut fb) = resolve_single_global_fallback(feat, g_data, targets) {
            if feat == HUMIDITY_CONTROL {
                preserve_local_sensor(&bound, &mut fb);
            }
            bound.insert(feat.to_owned(), fb);
        }
    }
    bound
}

// =============================================================================
/// Resolves effective `(preheat_active, sanitized_adaptive_cfg_map)` from `configs`.
#[must_use]
pub fn resolve_preheat_state(configs: &ScopeConfigMap) -> (bool, Map<String, Value>) {
    let (sch_on, sch_cfg) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
    let adp_entry = configs.get(ADAPTIVE_START);
    let adp_on = adp_entry
        .and_then(|e| e.get("is_active"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let adp_map: Map<String, Value> = adp_entry
        .and_then(|e| e.get("config"))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !PREHEAT_LEGACY_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    (sch_on && (sch_cfg.preheat_enabled || adp_on), adp_map)
}

// =============================================================================
/// Resolves the active scheduled base temperature (`°C`) if `HeatingSchedule` is active.
#[must_use]
pub fn resolve_scheduled_base_temp(configs: &ScopeConfigMap, now_dt: DateTime<Tz>) -> Option<f64> {
    let (is_on, cfg) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
    is_on.then(|| HeatingScheduleRule::resolve_active_slot(&cfg, now_dt).1)
}

// =============================================================================
/// Resolves upcoming `(target_epoch, target_temp)` if `HeatingSchedule` transitions higher.
#[must_use]
pub fn resolve_schedule_transition(
    configs: &ScopeConfigMap,
    now_dt: DateTime<Tz>,
) -> Option<(f64, f64)> {
    let (is_on, cfg) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
    if !is_on || cfg.days.is_empty() {
        return None;
    }
    let (_, cur_temp) = HeatingScheduleRule::resolve_active_slot(&cfg, now_dt);
    HeatingScheduleRule::get_next_comfort_transition(&cfg, now_dt)
        .filter(|&(_, next_t)| next_t > cur_temp)
}

// =============================================================================
/// Resolves the active `HeatingSchedule` `slot_id` for `ain` when latching manual overrides.
pub(super) async fn resolve_device_schedule_slot_id(
    ain: &str,
    db: &Database,
    registry: &StateRegistry,
    now_dt: DateTime<Tz>,
) -> Option<String> {
    let ain_owned = ain.to_owned();
    let norm_ain = normalize_ain(ain);
    let is_switch = registry
        .get_device(ain)
        .is_some_and(|d| d.device_type == device_types::SWITCH);

    let cfg_opt = db
        .run_blocking(move |d| {
            let mut sched_val = Value::Null;
            let mut preset_ain = ain_owned.clone();
            let mut in_group = false;
            if let Ok(groups) = d.get_all_software_groups() {
                for g in groups {
                    let members: Vec<String> = g
                        .get("members")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if members.iter().any(|m| normalize_ain(m) == norm_ain) {
                        if let Some(s) = g.get("automation").and_then(|a| a.get(HEATING_SCHEDULE)) {
                            sched_val = s.clone();
                        }
                        if let Some(first) = members.first() {
                            preset_ain.clone_from(first);
                        }
                        in_group = true;
                        break;
                    }
                }
            }
            let is_on = |v: &Value| v.get("is_active").and_then(Value::as_bool).unwrap_or(false);
            if !in_group
                && !is_on(&sched_val)
                && let Ok(dev_map) = d.get_all_automation_configs(SCOPE_DEVICE, &ain_owned)
            {
                sched_val = dev_map
                    .get(HEATING_SCHEDULE)
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            if !is_switch
                && !is_on(&sched_val)
                && let Ok(glob_map) = d.get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)
            {
                sched_val = glob_map
                    .get(HEATING_SCHEDULE)
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            if !is_on(&sched_val) {
                return Ok(None);
            }
            let raw_cfg = sched_val
                .get("config")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut norm_cfg = normalize_config_payload(HEATING_SCHEDULE, &raw_cfg);
            let (comfort, eco) = d.get_thermostat_presets(&preset_ain);
            if let Some(obj) = norm_cfg.as_object_mut() {
                obj.entry(String::from(KEY_COMFORT_TEMP))
                    .or_insert_with(|| json!(comfort));
                obj.entry(String::from(KEY_ECO_TEMP))
                    .or_insert_with(|| json!(eco));
            }
            let parsed: HeatingScheduleConfig =
                serde_json::from_value(norm_cfg).unwrap_or_default();
            Ok(Some(parsed))
        })
        .await
        .ok()
        .flatten()?;

    Some(HeatingScheduleRule::resolve_active_slot(&cfg_opt, now_dt).0)
}

// =============================================================================
/// Preserves a locally bound `sensor_ain` on `HumidityControl` when global fallback omits one.
fn preserve_local_sensor(bound: &ScopeConfigMap, fb: &mut Value) {
    let loc_s = bound
        .get(HUMIDITY_CONTROL)
        .and_then(|e| e.get("config"))
        .and_then(|c| c.get("sensor_ain"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if loc_s.is_empty() {
        return;
    }
    if let Some(fb_cfg) = fb.get_mut("config").and_then(Value::as_object_mut) {
        let empty_g = fb_cfg
            .get("sensor_ain")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty());
        if empty_g {
            fb_cfg.insert(String::from("sensor_ain"), json!(loc_s));
        }
    }
}

// =============================================================================
/// Populates target AIN fields on a single feature's `config` object.
fn bind_single_feature(feat: &str, cfg: &mut Map<String, Value>, targets: &[String]) {
    if matches!(
        feat,
        WEATHER_SMART_HEATING | PRESENCE_PROFILING | HEATING_SCHEDULE
    ) {
        cfg.insert(String::from("affected_devices"), json!(targets));
    } else if feat == HUMIDITY_CONTROL {
        cfg.insert(String::from("actor_ain"), json!(targets));
    } else if feat == APPARENT_TEMPERATURE {
        if let Some(s_ain) = cfg
            .get("sensor_ain")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
        {
            let mut map = cfg
                .get("sensor_mapping")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for t in targets {
                map.insert(t.clone(), json!(s_ain));
            }
            cfg.insert(String::from("sensor_mapping"), Value::Object(map));
        }
    } else if feat == ADAPTIVE_START && !targets.is_empty() {
        cfg.insert(String::from("target_ains"), json!(targets));
        if cfg
            .get("target_ain")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            cfg.insert(String::from("target_ain"), json!(targets[0]));
        }
    }
}

// =============================================================================
/// Projects a single `global` feature onto `targets`, returning `None` if inapplicable.
fn resolve_single_global_fallback(feat: &str, g_data: &Value, targets: &[String]) -> Option<Value> {
    let mut g_cfg = g_data
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let t_norms: HashSet<String> = targets.iter().map(|t| normalize_ain(t)).collect();

    if feat == APPARENT_TEMPERATURE {
        let sub: Map<String, Value> = g_cfg
            .get("sensor_mapping")
            .and_then(Value::as_object)?
            .iter()
            .filter(|(k, _)| t_norms.contains(&normalize_ain(k)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if sub.is_empty() {
            return None;
        }
        g_cfg.insert(String::from("sensor_mapping"), Value::Object(sub));
        return Some(json!({ "is_active": true, "config": g_cfg }));
    }
    if feat == ADAPTIVE_START {
        let raw_norms = extract_ain_set(&g_cfg, "target_ains").or_else(|| {
            g_cfg
                .get("target_ain")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| std::iter::once(normalize_ain(s)).collect())
        });
        let matched: Vec<String> = targets
            .iter()
            .filter(|t| {
                raw_norms
                    .as_ref()
                    .is_none_or(|set| set.contains(&normalize_ain(t)))
            })
            .cloned()
            .collect();
        if matched.is_empty() {
            return None;
        }
        g_cfg.insert(String::from("target_ain"), json!(matched[0]));
        g_cfg.insert(String::from("target_ains"), json!(matched));
        return Some(json!({ "is_active": true, "config": g_cfg }));
    }
    let key = if feat == HUMIDITY_CONTROL {
        "actor_ain"
    } else {
        "affected_devices"
    };
    let raw_norms = extract_ain_set(&g_cfg, key);
    let matched: Vec<String> = targets
        .iter()
        .filter(|t| {
            raw_norms
                .as_ref()
                .is_none_or(|set| set.contains(&normalize_ain(t)))
        })
        .cloned()
        .collect();
    if matched.is_empty() {
        return None;
    }
    g_cfg.insert(key.to_owned(), json!(matched));
    Some(json!({ "is_active": true, "config": g_cfg }))
}

// =============================================================================
/// Extracts a non-empty normalized AIN set from `cfg[key]`, or `None` when absent/empty.
fn extract_ain_set(cfg: &Map<String, Value>, key: &str) -> Option<HashSet<String>> {
    let arr = cfg.get(key)?.as_array()?;
    let set: HashSet<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(normalize_ain)
        .filter(|s| !s.is_empty())
        .collect();
    (!set.is_empty()).then_some(set)
}
