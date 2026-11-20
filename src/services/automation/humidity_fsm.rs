//! 4-state `HumidityControl` v2 FSM transitions (`IDLE`, `TRANSIENT_DEBOUNCE`,
//! `THERMAL_SHIELDING`, `RECOVERY_CHECK`) and $\max(T_{\text{guard}}, T_{\text{preheat}})$ actuation.

use super::actuation::{dispatch_thermostat_target, humidity_guard_states, RULE_TEMP_EPSILON};
use super::humidity::HumidityControlRule;
use super::humidity_helpers::{
    compute_stepdown_target, enrich_scope_psychrometrics, is_window_or_draft_active, ScopeClimate,
};
use crate::core::domain_constants::{
    analytics::history_defaults::FLOAT_EPSILON,
    automation::automation_defaults::{
        HUMIDITY_ACTION_TEMP_CAP, HUMIDITY_DURATION_MINUTES, HUMIDITY_HYSTERESIS_PERCENT,
        HUMIDITY_SUSTAIN_MINUTES, HUMIDITY_TRANSIENT_ABS_JUMP_GM3 as TRANSIENT_ABS_JUMP_GM3,
        HUMIDITY_TRANSIENT_RH_JUMP_PCT as TRANSIENT_RH_JUMP_PCT,
    },
    hardware::device_types,
};
use crate::core::psychrometrics::{
    calculate_guard_temperature, calculate_projected_base_humidity, ClimateGradients,
    GuardTempParams,
};
use crate::core::time_util::normalize_ain;
use crate::models::automation::HumidityControlConfig;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;

impl HumidityControlRule {
    /// Evaluates `RECOVERY_CHECK` vs continued `THERMAL_SHIELDING`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_active_shielding(
        &mut self,
        scope_key: &str,
        config: &HumidityControlConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        climate: ScopeClimate,
        base_temp: f64,
        d_t_10m: f64,
        scheduled_base: Option<f64>,
        now_ts: f64,
    ) {
        let phi_proj = calculate_projected_base_humidity(climate.0, climate.1, base_temp);
        let recovered =
            phi_proj <= (config.humidity_threshold - HUMIDITY_HYSTERESIS_PERCENT) + FLOAT_EPSILON;
        if recovered || is_window_or_draft_active(config, registry, d_t_10m) {
            self.release_scope_devices(&config.actor_ain, registry, aha, scheduled_base, true)
                .await;
            if recovered {
                self.high_humidity_since.remove(scope_key);
                self.transient_latched.remove(scope_key);
            }
            let next_st = if recovered {
                humidity_guard_states::IDLE
            } else {
                humidity_guard_states::TRANSIENT_DEBOUNCE
            };
            self.scope_states.insert(scope_key.to_owned(), next_st);
            enrich_scope_psychrometrics(config, registry, climate, base_temp, false, None, true);
            return;
        }
        self.apply_shielding_or_stepdown(
            scope_key, config, registry, aha, climate, base_temp, now_ts,
        )
        .await;
    }

    /// Evaluates transition from `IDLE` into `TRANSIENT_DEBOUNCE` or `THERMAL_SHIELDING`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_unshielded(
        &mut self,
        scope_key: &str,
        config: &HumidityControlConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        climate: ScopeClimate,
        base_temp: f64,
        gradients: ClimateGradients,
        now_ts: f64,
        heating_allowed: bool,
    ) {
        let phi_proj = calculate_projected_base_humidity(climate.0, climate.1, base_temp);
        let rec_limit = config.humidity_threshold - HUMIDITY_HYSTERESIS_PERCENT + FLOAT_EPSILON;
        if climate.1 <= config.humidity_threshold
            && (!self.high_humidity_since.contains_key(scope_key) || phi_proj <= rec_limit)
        {
            self.high_humidity_since.remove(scope_key);
            self.transient_latched.remove(scope_key);
            self.scope_states
                .insert(scope_key.to_owned(), humidity_guard_states::IDLE);
            enrich_scope_psychrometrics(config, registry, climate, base_temp, false, None, true);
            return;
        }
        let is_spike = gradients.delta_rh_15m >= (TRANSIENT_RH_JUMP_PCT - FLOAT_EPSILON)
            || gradients.delta_x_15m >= (TRANSIENT_ABS_JUMP_GM3 - FLOAT_EPSILON);
        let first_ts = *self
            .high_humidity_since
            .entry(scope_key.to_owned())
            .or_insert(gradients.first_exceeded_ts);
        let latched = self
            .transient_latched
            .get(scope_key)
            .copied()
            .unwrap_or(false)
            || is_spike;
        self.transient_latched.insert(scope_key.to_owned(), latched);

        let sustain_secs = (HUMIDITY_SUSTAIN_MINUTES as f64) * 60.0;
        if !heating_allowed
            || config.max_temp_boost_c <= 0.0
            || (latched && (now_ts - first_ts) < sustain_secs)
            || is_window_or_draft_active(config, registry, gradients.min_delta_t_10m)
        {
            let st = if !heating_allowed || config.max_temp_boost_c <= 0.0 {
                humidity_guard_states::IDLE
            } else {
                humidity_guard_states::TRANSIENT_DEBOUNCE
            };
            self.scope_states.insert(scope_key.to_owned(), st);
            enrich_scope_psychrometrics(config, registry, climate, base_temp, false, None, true);
            return;
        }
        self.apply_shielding_or_stepdown(
            scope_key, config, registry, aha, climate, base_temp, now_ts,
        )
        .await;
    }

    /// Computes $T_{\text{guard}}$ and actuates scope actors via $\max(T_{\text{guard}}, T_{\text{preheat}})$.
    #[allow(clippy::too_many_arguments)]
    async fn apply_shielding_or_stepdown(
        &mut self,
        scope_key: &str,
        config: &HumidityControlConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        climate: ScopeClimate,
        base_temp: f64,
        now_ts: f64,
    ) {
        let t_guard = calculate_guard_temperature(&GuardTempParams {
            temp_in_c: climate.0,
            rel_humidity_in_pct: climate.1,
            temp_base_c: base_temp,
            temp_out_c: climate.2,
            humidity_threshold: config.humidity_threshold,
            hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
            max_temp_boost_c: config.max_temp_boost_c,
            action_temp: HUMIDITY_ACTION_TEMP_CAP,
        });
        let (desired, state_name) = compute_stepdown_target(
            config,
            registry,
            &self.shielding_since,
            climate,
            base_temp,
            t_guard,
            now_ts,
        );
        self.scope_states.insert(scope_key.to_owned(), state_name);
        let guard_active = desired > base_temp + RULE_TEMP_EPSILON;
        let expiry = now_ts + (HUMIDITY_DURATION_MINUTES as f64) * 60.0;

        for ain in &config.actor_ain {
            let Some(actor) = registry.get_device(ain) else {
                continue;
            };
            if !matches!(
                actor.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            ) {
                continue;
            }
            let cur_t = actor.as_thermostat().map_or(0.0, |t| t.temperature_target);
            let is_manual = self
                .cycle_override_ains
                .contains(&normalize_ain(&actor.ain));
            let preheat_t = actor
                .as_thermostat()
                .filter(|t| t.preheat_active)
                .and_then(|t| t.preheat_target_temp)
                .unwrap_or(0.0);
            let eff_target = desired
                .max(preheat_t)
                .max(if is_manual { cur_t } else { 0.0 });
            let was_shielding = self.shielding_since.contains_key(&actor.ain);
            if guard_active {
                self.previous_targets
                    .entry(actor.ain.clone())
                    .or_insert(if base_temp > 0.0 { base_temp } else { cur_t });
                self.shielding_since
                    .entry(actor.ain.clone())
                    .or_insert(now_ts);
                self.boost_until.insert(actor.ain.clone(), expiry);
            } else {
                self.boost_until.remove(&actor.ain);
                self.shielding_since.remove(&actor.ain);
                self.previous_targets.remove(&actor.ain);
            }
            if (guard_active || was_shielding) && (cur_t - eff_target).abs() >= RULE_TEMP_EPSILON {
                self.logger.info(&format!(
                    "HumidityGuard [{}]: target {eff_target}°C (guard {desired}°C, base {base_temp}°C).",
                    actor.name
                ));
                if let Err(err) =
                    dispatch_thermostat_target(&actor, eff_target, registry, aha).await
                {
                    self.logger.warning(&format!(
                        "HumidityGuard target failed on {}: {err}",
                        actor.name
                    ));
                }
            }
        }
        enrich_scope_psychrometrics(
            config,
            registry,
            climate,
            base_temp,
            guard_active,
            None,
            true,
        );
    }
}
