//! 5-tier prioritized per-scope evaluation chain for [`super::RulesEngine`].
//!
//! Enforces intra-cycle precedence (`VacationMode` > `WeatherSmartHeating` >
//! `PresenceProfiling` > $\max(T_{\text{guard}}, T_{\text{preheat}})$ >
//! `HeatingSchedule` > `ApparentTemperature`).

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::Value;

use super::runner::RulesEngine;
use super::scopes::{
    bind_configs_to_targets, ensure_group_sensor_binding, merge_global_fallbacks,
    parse_feature_cfg, partition_group_members, resolve_scheduled_base_temp,
    sync_scope_thermostat_presets, ScopeConfigMap,
};
use super::trace::{build_scope_trace_entry, sync_scope_preheat_paused_reasons, RuleSet};
use crate::core::domain_constants::{
    automation::automation_identifiers::{
        APPARENT_TEMPERATURE, HEATING_SCHEDULE, PRESENCE_PROFILING, SCOPE_DEVICE, SCOPE_GROUP,
        WEATHER_SMART_HEATING,
    },
    hardware::device_types,
};
use crate::core::time_util::normalize_ain;
use crate::models::automation::{
    ApparentTemperatureConfig, HeatingScheduleConfig, PresenceProfilingConfig,
    WeatherSmartHeatingConfig,
};

impl RulesEngine {
    /// Evaluates all Software Group scopes (`Single Authority`) and returns grouped normalized AINs.
    pub(super) async fn evaluate_group_scopes(
        &self,
        rules: &mut RuleSet,
        groups: &[Value],
        global_configs: &ScopeConfigMap,
        vacation_locked: &HashSet<String>,
        scope_traces: &mut Vec<Value>,
    ) -> HashSet<String> {
        let grouped_ains: HashSet<String> = groups
            .iter()
            .flat_map(|g| extract_group_members(Some(g)))
            .map(|a| normalize_ain(&a))
            .collect();
        for group in groups {
            let members = extract_group_members(Some(group));
            let (targets, group_sensor) = partition_group_members(&members, &self.registry);
            if targets.is_empty() {
                continue;
            }
            let gid = group
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let mut grp_cfgs = group
                .get("automation")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            ensure_group_sensor_binding(&gid, &mut grp_cfgs, &group_sensor, &self.db, &self.logger)
                .await;
            let bound = bind_configs_to_targets(&grp_cfgs, &targets, &group_sensor);
            let merged = merge_global_fallbacks(bound, global_configs, &targets);
            self.evaluate_scope_chain(
                rules,
                merged,
                &targets,
                vacation_locked,
                false,
                SCOPE_GROUP,
                &gid,
                scope_traces,
            )
            .await;
        }
        grouped_ains
    }

    /// Evaluates standalone ungrouped `device` scopes and records them into `managed`.
    pub(super) async fn evaluate_device_scopes(
        &self,
        rules: &mut RuleSet,
        device_map: &HashMap<String, Value>,
        global_configs: &ScopeConfigMap,
        vacation_locked: &HashSet<String>,
        managed: &mut HashSet<String>,
        scope_traces: &mut Vec<Value>,
    ) {
        for (ain, dev_val) in device_map {
            let norm = normalize_ain(ain);
            if !managed.insert(norm) {
                continue;
            }
            let is_switch = self
                .registry
                .get_device(ain)
                .is_some_and(|d| d.device_type == device_types::SWITCH);
            let raw_map = dev_val.as_object().cloned().unwrap_or_default();
            let slice = [ain.clone()];
            let bound = bind_configs_to_targets(&raw_map, &slice, "");
            let merged = if is_switch {
                bound
            } else {
                merge_global_fallbacks(bound, global_configs, &slice)
            };
            self.evaluate_scope_chain(
                rules,
                merged,
                &slice,
                vacation_locked,
                false,
                SCOPE_DEVICE,
                ain,
                scope_traces,
            )
            .await;
        }
    }

    /// Executes the 5-tier prioritized rule chain for one scope.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn evaluate_scope_chain(
        &self,
        rules: &mut RuleSet,
        mut configs: ScopeConfigMap,
        targets: &[String],
        vacation_locked: &HashSet<String>,
        is_global: bool,
        scope_type: &str,
        scope_id: &str,
        scope_traces: &mut Vec<Value>,
    ) {
        if !is_global {
            let (app_on, app_cfg) =
                parse_feature_cfg::<ApparentTemperatureConfig>(&configs, APPARENT_TEMPERATURE);
            if app_on {
                rules.apparent_temp.enrich_only(&app_cfg, &self.registry);
            }
        }
        sync_scope_thermostat_presets(targets, &mut configs, &self.registry, &self.db).await;
        let now_dt = self.now_local();
        let unlocked: Vec<String> = targets
            .iter()
            .filter(|t| !vacation_locked.contains(&normalize_ain(t)))
            .cloned()
            .collect();

        let non_summer = self
            .run_weather_step(rules, &configs, &unlocked, is_global, now_dt)
            .await;
        let present = self
            .run_presence_step(rules, &configs, &non_summer, now_dt)
            .await;
        let post_preheat = self
            .run_adaptive_start_step(rules, &configs, targets, &present, is_global, now_dt)
            .await;
        let sched_targets = self
            .run_humidity_step(
                rules,
                &configs,
                &non_summer,
                &post_preheat,
                is_global,
                targets,
                now_dt,
            )
            .await;
        self.run_schedule_step(
            rules,
            &configs,
            targets,
            &present,
            &sched_targets,
            is_global,
            now_dt,
        )
        .await;
        sync_scope_preheat_paused_reasons(
            rules,
            &configs,
            targets,
            vacation_locked,
            is_global,
            &self.registry,
            now_dt,
        );
        self.apply_scope_offset_compensation(rules, &configs, &unlocked)
            .await;

        if let Some(entry) = build_scope_trace_entry(
            rules,
            &configs,
            targets,
            &sched_targets,
            scope_type,
            scope_id,
            &self.registry,
            &self.db,
            self.weather_service.as_deref(),
            now_dt,
        )
        .await
        {
            scope_traces.push(entry);
        }
    }

    /// Runs `WeatherSmartHeating` (`Summer Pause`) and returns unsuppressed targets.
    pub(super) async fn run_weather_step(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        unlocked: &[String],
        is_global: bool,
        now_dt: DateTime<Tz>,
    ) -> Vec<String> {
        for t in unlocked {
            rules.weather.last_suppressed_ains.remove(&normalize_ain(t));
        }
        let (wea_on, mut wea_cfg) =
            parse_feature_cfg::<WeatherSmartHeatingConfig>(configs, WEATHER_SMART_HEATING);
        let (sch_on, _) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
        if !wea_on {
            if !sch_on {
                rules
                    .weather
                    .restore_devices(unlocked, &self.registry, &self.aha)
                    .await;
            }
        } else if !unlocked.is_empty() || is_global {
            wea_cfg.affected_devices = filter_target_list(&wea_cfg.affected_devices, unlocked);
            rules
                .weather
                .evaluate(
                    &wea_cfg,
                    &self.registry,
                    self.weather_service.as_deref(),
                    &self.aha,
                    now_dt,
                )
                .await;
        }
        unlocked
            .iter()
            .filter(|t| {
                !rules
                    .weather
                    .last_suppressed_ains
                    .contains(&normalize_ain(t))
            })
            .cloned()
            .collect()
    }

    /// Runs `PresenceProfiling` and returns targets not in away setback.
    pub(super) async fn run_presence_step(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        active_targets: &[String],
        now_dt: DateTime<Tz>,
    ) -> Vec<String> {
        let (prs_on, mut prs_cfg) =
            parse_feature_cfg::<PresenceProfilingConfig>(configs, PRESENCE_PROFILING);
        if prs_cfg.tracked_mac_addresses.is_empty() {
            return active_targets.to_vec();
        }
        prs_cfg.affected_devices = filter_target_list(&prs_cfg.affected_devices, active_targets);
        let eff_active = prs_on && !prs_cfg.affected_devices.is_empty();
        let sched_base = resolve_scheduled_base_temp(configs, now_dt);
        rules
            .presence
            .evaluate(
                &prs_cfg,
                eff_active,
                &self.registry,
                &self.db,
                &self.aha,
                &self.tr064,
                sched_base,
            )
            .await;
        active_targets
            .iter()
            .filter(|t| !rules.presence.last_setback_ains.contains(&normalize_ain(t)))
            .cloned()
            .collect()
    }

    /// Evaluates `HeatingSchedule` for `sched_targets` and anchors pre-heating slots.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_schedule_step(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        all_targets: &[String],
        unsuppressed: &[String],
        sched_targets: &[String],
        is_global: bool,
        now_dt: DateTime<Tz>,
    ) {
        let (sch_on, mut sch_cfg) =
            parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
        if !sch_on {
            if !all_targets.is_empty() {
                rules.schedule.invalidate_devices(all_targets);
            }
            return;
        }
        let unsup_norm: HashSet<String> = unsuppressed.iter().map(|t| normalize_ain(t)).collect();
        let overridden: Vec<String> = all_targets
            .iter()
            .filter(|t| !unsup_norm.contains(&normalize_ain(t)))
            .cloned()
            .collect();
        if !overridden.is_empty() {
            rules.schedule.invalidate_devices(&overridden);
        }
        let sched_norm: HashSet<String> = sched_targets.iter().map(|t| normalize_ain(t)).collect();
        let preheating: Vec<String> = unsuppressed
            .iter()
            .filter(|t| !sched_norm.contains(&normalize_ain(t)))
            .cloned()
            .collect();
        if !preheating.is_empty() {
            rules
                .schedule
                .anchor_devices_slot(&preheating, &sch_cfg, now_dt);
        }
        if !sched_targets.is_empty() || is_global {
            sch_cfg.affected_devices = filter_target_list(&sch_cfg.affected_devices, sched_targets);
            rules
                .schedule
                .evaluate(&sch_cfg, &self.registry, &self.aha, now_dt)
                .await;
        }
    }

    /// Applies room-sensor dynamic offset compensation to unlocked scope thermostats.
    pub(super) async fn apply_scope_offset_compensation(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        allowed_targets: &[String],
    ) {
        if allowed_targets.is_empty() {
            return;
        }
        let (app_on, mut app_cfg) =
            parse_feature_cfg::<ApparentTemperatureConfig>(configs, APPARENT_TEMPERATURE);
        if !app_on || app_cfg.sensor_mapping.is_empty() {
            return;
        }
        let allowed_norm: HashSet<String> =
            allowed_targets.iter().map(|t| normalize_ain(t)).collect();
        app_cfg
            .sensor_mapping
            .retain(|k, _| allowed_norm.contains(&normalize_ain(k)));
        if !app_cfg.sensor_mapping.is_empty() {
            rules
                .apparent_temp
                .evaluate(&app_cfg, &self.registry, &self.aha)
                .await;
        }
    }
}

// =============================================================================
/// Filters `configured` (or defaults to `allowed` when empty) against `allowed` normalized AINs.
#[must_use]
pub(super) fn filter_target_list(configured: &[String], allowed: &[String]) -> Vec<String> {
    let allowed_norm: HashSet<String> = allowed.iter().map(|t| normalize_ain(t)).collect();
    let source = if configured.is_empty() {
        allowed
    } else {
        configured
    };
    source
        .iter()
        .filter(|t| allowed_norm.contains(&normalize_ain(t)))
        .cloned()
        .collect()
}

// =============================================================================
/// Extracts trimmed non-empty `"members"` AIN strings from a software group JSON value.
pub(super) fn extract_group_members(group: Option<&Value>) -> Vec<String> {
    group
        .and_then(|g| g.get("members"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
