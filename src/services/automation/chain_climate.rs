//! `AdaptiveStart` & `HumidityControl` v2 scope steps inside `RulesEngine`.
//!
//! Separated from [`super::chain`] so the predictive pre-heat and `DIN 4108-2`
//! $\max(T_{\text{guard}}, T_{\text{preheat}})$ steps stay concise (`<= 300` lines).

use std::collections::HashSet;

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::Value;

use super::chain::filter_target_list;
use super::humidity::HumidityEvalParams;
use super::runner::RulesEngine;
use super::scopes::{
    parse_feature_cfg, resolve_preheat_state, resolve_schedule_transition,
    resolve_scheduled_base_temp, ScopeConfigMap,
};
use super::trace::RuleSet;
use crate::core::domain_constants::automation::automation_identifiers::{
    HEATING_SCHEDULE, HUMIDITY_CONTROL,
};
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::models::automation::{
    AdaptiveStartConfig, HeatingScheduleConfig, HumidityControlConfig,
};

impl RulesEngine {
    /// Runs `AdaptiveStart` (`Pre-Heat`) and returns targets not currently pre-heating.
    pub(super) async fn run_adaptive_start_step(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        all_targets: &[String],
        active_targets: &[String],
        is_global: bool,
        now_dt: DateTime<Tz>,
    ) -> Vec<String> {
        if all_targets.is_empty() {
            return active_targets.to_vec();
        }
        let now_ts = now_epoch_f64();
        let win_rel: Vec<String> = all_targets
            .iter()
            .filter(|t| {
                rules.adaptive_start.is_device_preheating(t, now_ts)
                    && self
                        .registry
                        .get_device(t)
                        .and_then(|d| d.as_thermostat().map(|tm| tm.window_open))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !win_rel.is_empty() {
            rules.schedule.invalidate_devices(&win_rel);
        }
        let active_norm: HashSet<String> =
            active_targets.iter().map(|t| normalize_ain(t)).collect();
        let suppressed: Vec<String> = all_targets
            .iter()
            .filter(|t| !active_norm.contains(&normalize_ain(t)))
            .cloned()
            .collect();
        if !suppressed.is_empty() {
            rules
                .adaptive_start
                .clear_devices(&suppressed, false, Some(&self.registry));
        }
        let overridden: Vec<String> = active_targets
            .iter()
            .filter(|t| rules.schedule.is_manual_override_active(t))
            .cloned()
            .collect();
        if !overridden.is_empty() {
            rules
                .adaptive_start
                .clear_devices(&overridden, true, Some(&self.registry));
        }
        let mut eligible: Vec<String> = active_targets
            .iter()
            .filter(|t| !rules.schedule.is_manual_override_active(t))
            .cloned()
            .collect();

        let (preheat_on, adp_map) = resolve_preheat_state(configs);
        let mut adp_cfg: AdaptiveStartConfig =
            serde_json::from_value(Value::Object(adp_map)).unwrap_or_default();
        if is_global && (!adp_cfg.target_ains.is_empty() || !adp_cfg.target_ain.trim().is_empty()) {
            let exp = if !adp_cfg.target_ains.is_empty() {
                adp_cfg.target_ains.clone()
            } else {
                vec![adp_cfg.target_ain.clone()]
            };
            eligible = filter_target_list(&exp, &eligible);
        }
        if !preheat_on || eligible.is_empty() {
            rules
                .adaptive_start
                .clear_devices(all_targets, preheat_on, Some(&self.registry));
            return active_targets.to_vec();
        }
        adp_cfg.target_ain = eligible[0].clone();
        adp_cfg.target_ains = eligible;
        let next_trans = resolve_schedule_transition(configs, now_dt);
        rules
            .adaptive_start
            .evaluate(
                &adp_cfg,
                true,
                &self.registry,
                &self.db,
                self.weather_service.as_deref(),
                &self.aha,
                next_trans,
                now_dt.timezone(),
            )
            .await;
        let post_ts = now_epoch_f64();
        active_targets
            .iter()
            .filter(|t| !rules.adaptive_start.is_device_preheating(t, post_ts))
            .cloned()
            .collect()
    }

    /// Runs `HumidityControl` v2 with $T_{\text{eff}} = \max(T_{\text{guard}}, T_{\text{preheat}})$.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_humidity_step(
        &self,
        rules: &mut RuleSet,
        configs: &ScopeConfigMap,
        non_summer: &[String],
        post_preheat: &[String],
        is_global: bool,
        all_targets: &[String],
        now_dt: DateTime<Tz>,
    ) -> Vec<String> {
        let prev_guarded = rules.humidity.active_boost_ains();
        let (hum_on, raw_cfg) =
            parse_feature_cfg::<HumidityControlConfig>(configs, HUMIDITY_CONTROL);
        let mut hum_cfg = raw_cfg.sanitized();
        let allowed_norm: HashSet<String> = non_summer.iter().map(|t| normalize_ain(t)).collect();
        let blocked: Vec<String> = all_targets
            .iter()
            .filter(|t| !allowed_norm.contains(&normalize_ain(t)))
            .cloned()
            .collect();
        if !blocked.is_empty() {
            rules
                .humidity
                .release_scope_devices(&blocked, &self.registry, &self.aha, None, false)
                .await;
        }
        let current = if hum_cfg.actor_ain.is_empty() {
            all_targets.to_vec()
        } else if is_global {
            filter_target_list(&hum_cfg.actor_ain, all_targets)
        } else {
            hum_cfg.actor_ain.clone()
        };
        let eligible: Vec<String> = current
            .iter()
            .filter(|t| allowed_norm.contains(&normalize_ain(t)))
            .cloned()
            .collect();
        hum_cfg.actor_ain = if eligible.is_empty() {
            current
        } else {
            eligible.clone()
        };
        let (sch_on, _) = parse_feature_cfg::<HeatingScheduleConfig>(configs, HEATING_SCHEDULE);
        let overrides = if sch_on {
            rules.schedule.manual_override_ains()
        } else {
            HashSet::new()
        };
        let sched_base = resolve_scheduled_base_temp(configs, now_dt);

        if hum_on && (!all_targets.is_empty() || !hum_cfg.sensor_ain.trim().is_empty()) {
            let p = HumidityEvalParams {
                scheduled_base_temp: sched_base,
                heating_allowed: !eligible.is_empty(),
                manual_override_ains: &overrides,
            };
            rules
                .humidity
                .evaluate(
                    &hum_cfg,
                    &self.registry,
                    self.weather_service.as_deref(),
                    &self.aha,
                    &self.db,
                    p,
                )
                .await;
        } else {
            rules.humidity.cycle_override_ains = overrides;
            if !non_summer.is_empty() {
                rules
                    .humidity
                    .release_scope_devices(non_summer, &self.registry, &self.aha, sched_base, true)
                    .await;
            }
            rules.humidity.enrich_passive_scope(
                &hum_cfg,
                &self.registry,
                self.weather_service.as_deref(),
                sched_base,
            );
        }

        let boosting = rules.humidity.active_boost_ains();
        let released_away: Vec<String> = non_summer
            .iter()
            .filter(|t| {
                let n = normalize_ain(t);
                prev_guarded.contains(&n)
                    && !boosting.contains(&n)
                    && rules.presence.last_setback_ains.contains(&n)
            })
            .cloned()
            .collect();
        if !released_away.is_empty() {
            drop(
                self.run_presence_step(rules, configs, &released_away, now_dt)
                    .await,
            );
        }
        post_preheat
            .iter()
            .filter(|t| !boosting.contains(&normalize_ain(t)))
            .cloned()
            .collect()
    }
}
