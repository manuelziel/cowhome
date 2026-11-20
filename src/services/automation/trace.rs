//! Home Assistant-style per-scope & cycle-wide `RulesEngine` decision tracing.
//!
//! Records structured evaluation snapshots (`get_last_evaluation_trace`) and synchronizes
//! `preheat_paused_reason` across standalone and grouped thermostats.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::{json, Value};

use super::actuation::preheat_paused_reasons;
use super::adaptive_start::AdaptiveStartRule;
use super::apparent_temp::ApparentTemperatureRule;
use super::climate::{VacationModeRule, WeatherSmartHeatingRule};
use super::humidity::HumidityControlRule;
use super::presence::PresenceProfilingRule;
use super::schedule::HeatingScheduleRule;
pub(super) use super::scope_binding::resolve_device_schedule_slot_id;
use super::scopes::{
    parse_feature_cfg, resolve_preheat_state, resolve_schedule_transition,
    resolve_scheduled_base_temp, ScopeConfigMap,
};
use crate::core::domain_constants::{
    automation::automation_identifiers::{
        ADAPTIVE_START, APPARENT_TEMPERATURE, HEATING_SCHEDULE, HUMIDITY_CONTROL,
        PRESENCE_PROFILING, VACATION_MODE, WEATHER_SMART_HEATING,
    },
    hardware::{device_types, special_devices::GROUP_ID_PREFIX},
};
use crate::core::logger::Logger;
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::automation::{
    AdaptiveStartConfig, ApparentTemperatureConfig, HeatingScheduleConfig, HumidityControlConfig,
    PresenceProfilingConfig, VacationModeConfig, WeatherSmartHeatingConfig,
};
use crate::models::registry::StateRegistry;
use crate::services::weather::WeatherService;

// =============================================================================
/// Container owning all 7 stateful automation rule evaluators inside [`super::RulesEngine`].
#[derive(Debug)]
pub(super) struct RuleSet {
    /// Seasonal `Summer Pause` rule (`WeatherSmartHeating`).
    pub weather: WeatherSmartHeatingRule,
    /// `DIN 4108-2` `HumidityControl` v2 FSM rule.
    pub humidity: HumidityControlRule,
    /// House-wide `VacationMode` (`Frost Guard`) rule.
    pub vacation: VacationModeRule,
    /// Master-slave room climate sensor sync & static calibration rule.
    pub apparent_temp: ApparentTemperatureRule,
    /// 7-day / 48-slot `HeatingSchedule` & `PowerSchedule` rule.
    pub schedule: HeatingScheduleRule,
    /// `DIN EN 12098-1` predictive pre-heating rule (`AdaptiveStart`).
    pub adaptive_start: AdaptiveStartRule,
    /// TR-064 Wi-Fi presence auto-away rule (`PresenceProfiling`).
    pub presence: PresenceProfilingRule,
}

impl RuleSet {
    /// Instantiates all 7 stateful automation rule instances.
    #[must_use]
    pub fn new(logger: &Logger) -> Self {
        Self {
            weather: WeatherSmartHeatingRule::new(logger.clone()),
            humidity: HumidityControlRule::new(logger.clone()),
            vacation: VacationModeRule::new(logger.clone()),
            apparent_temp: ApparentTemperatureRule::new(logger.clone()),
            schedule: HeatingScheduleRule::new(logger.clone()),
            adaptive_start: AdaptiveStartRule::new(logger.clone()),
            presence: PresenceProfilingRule::new(logger.clone()),
        }
    }
}

// =============================================================================
/// Synchronizes `preheat_paused_reason` across all thermostats in an evaluated scope.
pub(super) fn sync_scope_preheat_paused_reasons(
    rules: &RuleSet,
    configs: &ScopeConfigMap,
    targets: &[String],
    vacation_locked: &HashSet<String>,
    is_global: bool,
    registry: &StateRegistry,
    now_dt: DateTime<Tz>,
) {
    if targets.is_empty() {
        return;
    }
    let (preheat_on, adp_map) = resolve_preheat_state(configs);
    let exp_norm: Option<HashSet<String>> = if is_global {
        let adp_cfg: AdaptiveStartConfig =
            serde_json::from_value(Value::Object(adp_map)).unwrap_or_default();
        let list: Vec<&str> = if !adp_cfg.target_ains.is_empty() {
            adp_cfg.target_ains.iter().map(String::as_str).collect()
        } else if !adp_cfg.target_ain.trim().is_empty() {
            vec![adp_cfg.target_ain.as_str()]
        } else {
            Vec::new()
        };
        (!list.is_empty()).then(|| list.into_iter().map(normalize_ain).collect())
    } else {
        None
    };
    let next_epoch = preheat_on
        .then(|| resolve_schedule_transition(configs, now_dt))
        .flatten()
        .map(|(ep, _)| ep);
    let now_ts = now_epoch_f64();

    for ain in targets {
        let in_scope = preheat_on
            && exp_norm
                .as_ref()
                .is_none_or(|set| set.contains(&normalize_ain(ain)));
        let reason = if in_scope {
            resolve_device_paused_reason(rules, ain, vacation_locked, next_epoch, now_ts, registry)
        } else {
            None
        };
        registry.mutate_device(ain, |d| {
            if let Some(t) = d.as_thermostat_mut() {
                t.preheat_paused_reason = reason.map(str::to_owned);
            }
        });
    }
}

// =============================================================================
/// Aggregates the first non-null member `preheat_paused_reason` onto `thermostat_group` twins.
pub(super) fn sync_thermostat_group_paused_reasons(groups: &[Value], registry: &StateRegistry) {
    let all_devs = registry.get_all_devices();
    let db_members: HashMap<String, HashSet<String>> = groups
        .iter()
        .filter_map(|g| {
            let gid = g.get("id")?.as_str()?;
            let pref = if gid.starts_with(GROUP_ID_PREFIX) {
                gid.to_owned()
            } else {
                format!("{GROUP_ID_PREFIX}{gid}")
            };
            let set: HashSet<String> = g
                .get("members")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(normalize_ain)
                .collect();
            Some((normalize_ain(&pref), set))
        })
        .collect();

    for grp_dev in &all_devs {
        if grp_dev.device_type != device_types::THERMOSTAT_GROUP {
            continue;
        }
        if grp_dev.as_thermostat().is_some_and(|t| t.preheat_active) {
            registry.mutate_device(&grp_dev.ain, |d| {
                if let Some(t) = d.as_thermostat_mut() {
                    t.preheat_paused_reason = None;
                }
            });
            continue;
        }
        let g_norm = normalize_ain(&grp_dev.ain);
        let known = db_members.get(&g_norm);
        let member_reason = all_devs
            .iter()
            .filter(|m| m.ain != grp_dev.ain)
            .filter(|m| {
                let m_norm = normalize_ain(&m.ain);
                known.is_some_and(|s| s.contains(&m_norm))
                    || m.group_id
                        .as_deref()
                        .is_some_and(|gid| normalize_ain(gid) == g_norm)
            })
            .find_map(|m| m.as_thermostat()?.preheat_paused_reason.clone());
        registry.mutate_device(&grp_dev.ain, |d| {
            if let Some(t) = d.as_thermostat_mut() {
                t.preheat_paused_reason = member_reason;
            }
        });
    }
}

// =============================================================================
/// Builds a structured per-scope diagnostic decision trace JSON object.
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_scope_trace_entry(
    rules: &RuleSet,
    configs: &ScopeConfigMap,
    targets: &[String],
    sched_targets: &[String],
    scope_type: &str,
    scope_id: &str,
    registry: &StateRegistry,
    db: &Database,
    weather_service: Option<&WeatherService>,
    now_dt: DateTime<Tz>,
) -> Option<Value> {
    let mut active_feats: Vec<String> = configs
        .iter()
        .filter(|(_, d)| d.get("is_active").and_then(Value::as_bool).unwrap_or(false))
        .map(|(f, _)| f.clone())
        .collect();
    let (preheat_on, adp_map) = resolve_preheat_state(configs);
    if preheat_on && !active_feats.iter().any(|f| f == ADAPTIVE_START) {
        active_feats.push(ADAPTIVE_START.to_owned());
    }
    if targets.is_empty() && active_feats.is_empty() {
        return None;
    }
    let next_trans = resolve_schedule_transition(configs, now_dt);
    let sched_base = resolve_scheduled_base_temp(configs, now_dt);
    let rules_map = build_scope_rules_map(
        rules,
        configs,
        targets,
        preheat_on,
        adp_map,
        next_trans,
        sched_base,
        registry,
        db,
        weather_service,
        now_dt,
    )
    .await;

    Some(json!({
        "scope_type": scope_type,
        "scope_id": scope_id,
        "targets": targets,
        "schedule_eligible_targets": sched_targets,
        "active_features": active_feats,
        "scheduled_base_temp": sched_base,
        "next_comfort_transition": next_trans.map(|(ep, tt)| json!({ "target_epoch": ep, "target_temp": tt })),
        "rules": rules_map,
    }))
}

// =============================================================================
/// Builds the top-level `last_evaluation_trace` snapshot at the end of a cycle.
#[must_use]
pub(super) fn build_cycle_evaluation_trace(
    rules: &RuleSet,
    vacation_locked: &HashSet<String>,
    scope_traces: Vec<Value>,
    registry: &StateRegistry,
    now_dt: DateTime<Tz>,
) -> Value {
    let now_ts = now_epoch_f64();
    let all_devs = registry.get_all_devices();
    let preheat_ains = rules.adaptive_start.active_preheating_ains(now_ts);
    let preheat_targets: serde_json::Map<String, Value> = all_devs
        .iter()
        .filter_map(|d| {
            let t = d.as_thermostat().filter(|t| t.preheat_active)?;
            Some((
                normalize_ain(&d.ain),
                json!({
                    "target_temp": t.preheat_target_temp,
                    "target_time": t.preheat_target_time,
                }),
            ))
        })
        .collect();
    let preheat_paused: serde_json::Map<String, Value> = all_devs
        .iter()
        .filter_map(|d| {
            let r = d.as_thermostat()?.preheat_paused_reason.as_ref()?;
            Some((normalize_ain(&d.ain), json!(r)))
        })
        .collect();

    json!({
        "evaluated_at_epoch": round_decimals(now_ts, 3),
        "evaluated_at_local_iso": now_dt.to_rfc3339(),
        "timezone_iana": now_dt.timezone().name(),
        "vacation_locked_ains": sorted_vec(vacation_locked),
        "humidity_boosting_ains": sorted_vec(&rules.humidity.active_boost_ains()),
        "summer_pause_suppressed_ains": sorted_vec(&rules.weather.last_suppressed_ains),
        "presence_away_ains": sorted_vec(&rules.presence.last_setback_ains),
        "adaptive_preheating_ains": preheat_ains,
        "adaptive_preheat_targets": preheat_targets,
        "adaptive_preheat_paused_reasons": preheat_paused,
        "manual_override_ains": sorted_vec(&rules.schedule.manual_override_ains()),
        "active_schedule_slots": rules.schedule.active_slots_snapshot(),
        "scopes": scope_traces,
    })
}

// =============================================================================
/// Resolves the highest-priority `preheat_paused_reasons` token for `ain`.
fn resolve_device_paused_reason(
    rules: &RuleSet,
    ain: &str,
    vacation_locked: &HashSet<String>,
    next_epoch: Option<f64>,
    now_ts: f64,
    registry: &StateRegistry,
) -> Option<&'static str> {
    let dev = registry.get_device(ain)?;
    if dev.as_thermostat().is_some_and(|t| t.preheat_active) {
        return None;
    }
    let norm = normalize_ain(ain);
    if vacation_locked.contains(&norm) {
        return Some(preheat_paused_reasons::VACATION_MODE);
    }
    if rules.weather.last_suppressed_ains.contains(&norm) {
        return Some(preheat_paused_reasons::SUMMER_PAUSE);
    }
    if rules.presence.last_setback_ains.contains(&norm) {
        return Some(preheat_paused_reasons::AUTO_AWAY);
    }
    if dev.as_thermostat().is_some_and(|t| t.window_open) {
        return Some(preheat_paused_reasons::WINDOW_OPEN);
    }
    if next_epoch.is_some()
        && (rules.schedule.is_manual_override_active(ain)
            || rules
                .adaptive_start
                .is_epoch_aborted(ain, next_epoch, now_ts))
    {
        return Some(preheat_paused_reasons::MANUAL_OVERRIDE);
    }
    None
}

// =============================================================================
/// Builds the 7-feature `"rules"` map inside a per-scope decision trace entry.
#[allow(clippy::too_many_arguments)]
async fn build_scope_rules_map(
    rules: &RuleSet,
    configs: &ScopeConfigMap,
    targets: &[String],
    preheat_on: bool,
    mut adp_map: serde_json::Map<String, Value>,
    next_trans: Option<(f64, f64)>,
    sched_base: Option<f64>,
    registry: &StateRegistry,
    db: &Database,
    weather_service: Option<&WeatherService>,
    now_dt: DateTime<Tz>,
) -> Value {
    let norm_targets: HashSet<String> = targets.iter().map(|a| normalize_ain(a)).collect();
    let (sch_on, sch_cfg) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
    let (slot_id, slot_t) = HeatingScheduleRule::resolve_active_slot(&sch_cfg, now_dt);
    let overrides = if sch_on {
        sorted_vec(
            &norm_targets
                .intersection(&rules.schedule.manual_override_ains())
                .cloned()
                .collect(),
        )
    } else {
        Vec::new()
    };
    if !targets.is_empty() {
        adp_map.insert(String::from("target_ains"), json!(targets));
    }
    let adp_cfg: AdaptiveStartConfig =
        serde_json::from_value(Value::Object(adp_map)).unwrap_or_default();
    let adp_trace = rules
        .adaptive_start
        .describe_scope_trace(
            &adp_cfg,
            preheat_on,
            registry,
            db,
            next_trans,
            now_dt,
            weather_service,
        )
        .await;

    let (wea_on, wea_cfg) =
        parse_feature_cfg::<WeatherSmartHeatingConfig>(configs, WEATHER_SMART_HEATING);
    let (app_on, app_cfg) =
        parse_feature_cfg::<ApparentTemperatureConfig>(configs, APPARENT_TEMPERATURE);
    let (hum_on, hum_cfg) = parse_feature_cfg::<HumidityControlConfig>(configs, HUMIDITY_CONTROL);
    let (vac_on, vac_cfg) = parse_feature_cfg::<VacationModeConfig>(configs, VACATION_MODE);
    let (prs_on, prs_cfg) =
        parse_feature_cfg::<PresenceProfilingConfig>(configs, PRESENCE_PROFILING);

    let vac_locked = sorted_vec(
        &norm_targets
            .intersection(&rules.vacation.locked_ains)
            .cloned()
            .collect(),
    );
    let prs_away = sorted_vec(
        &norm_targets
            .intersection(&rules.presence.last_setback_ains)
            .cloned()
            .collect(),
    );

    json!({
        HEATING_SCHEDULE: {
            "is_active": sch_on,
            "active_slot": sch_on.then_some(slot_id),
            "target_temp": sch_on.then_some(slot_t),
            "manual_override_ains": overrides,
            "preheat_enabled": preheat_on,
        },
        ADAPTIVE_START: adp_trace,
        WEATHER_SMART_HEATING: rules.weather.describe_scope_trace(&wea_cfg, wea_on, weather_service, now_dt),
        APPARENT_TEMPERATURE: rules.apparent_temp.describe_scope_trace(&app_cfg, app_on, targets, registry),
        HUMIDITY_CONTROL: rules.humidity.describe_scope_trace(
            &hum_cfg.sanitized(), hum_on, targets, registry, weather_service, sched_base
        ),
        VACATION_MODE: {
            "is_active": vac_on,
            "frost_protection_temp": vac_cfg.frost_protection_temp,
            "end_date_timestamp": vac_cfg.end_date_timestamp,
            "locked_ains": vac_locked,
        },
        PRESENCE_PROFILING: {
            "is_active": prs_on,
            "learning_mode_only": prs_cfg.learning_mode_only,
            "away_temp_offset": prs_cfg.away_temp_offset,
            "away_ains": prs_away,
        },
    })
}

// =============================================================================
/// Converts a `HashSet<String>` into a lexicographically sorted `Vec<String>`.
fn sorted_vec(set: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = set.iter().cloned().collect();
    out.sort();
    out
}
