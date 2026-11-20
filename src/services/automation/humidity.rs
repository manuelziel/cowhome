//! Building-physics `HumidityControl` v2 (`Humidity Guard`) per `DIN 4108-2`.
//!
//! Coordinates scope entry/release, expired boost restoration, gradient computation,
//! and diagnostic tracing, delegating FSM state transitions to [`super::humidity_fsm`].

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::actuation::{dispatch_thermostat_target, RULE_TEMP_EPSILON};
use super::humidity_helpers::{
    build_humidity_scope_trace, build_scope_key, enrich_scope_psychrometrics,
    load_recent_sql_samples, outdoor_climate, resolve_scope_base_temp, resolve_scope_climate,
    ScopeClimate,
};
use crate::core::domain_constants::automation::automation_defaults::{
    HUMIDITY_SAMPLE_RETENTION_SECS as SAMPLE_RETENTION_SECS, HUMIDITY_SUSTAIN_MINUTES,
};
use crate::core::logger::Logger;
use crate::core::psychrometrics::{compute_climate_gradients, ClimateGradients, ClimateSample};
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::automation::HumidityControlConfig;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;
use crate::services::weather::WeatherService;

// =============================================================================
/// Evaluation context passed to [`HumidityControlRule::evaluate`].
pub struct HumidityEvalParams<'a> {
    /// Active `HeatingSchedule` slot base temperature, if scheduled.
    pub scheduled_base_temp: Option<f64>,
    /// False when `VacationMode` or `Summer Pause` blocks heating boosts.
    pub heating_allowed: bool,
    /// Normalized AINs holding an active manual user schedule override.
    pub manual_override_ains: &'a HashSet<String>,
}

// =============================================================================
/// Stateful `DIN 4108-2` `HumidityControl` v2 rule.
#[derive(Debug)]
pub struct HumidityControlRule {
    /// Active boost expiration epochs (`s`) per raw device AIN.
    pub(super) boost_until: HashMap<String, f64>,
    /// Saved pre-boost baseline target temperatures (`°C`) per raw device AIN.
    pub(super) previous_targets: HashMap<String, f64>,
    /// Timestamp (`s`) when continuous thermal shielding started per raw device AIN.
    pub(super) shielding_since: HashMap<String, f64>,
    /// Earliest exceedance timestamp (`s`) per scope key.
    pub(super) high_humidity_since: HashMap<String, f64>,
    /// Whether a transient spike was latched during the current exceedance per scope key.
    pub(super) transient_latched: HashMap<String, bool>,
    /// Current 4-state FSM identifier per scope key.
    pub(super) scope_states: HashMap<String, &'static str>,
    /// Rolling 30-minute indoor climate samples per normalized sensor/actor key.
    recent_samples: HashMap<String, Vec<ClimateSample>>,
    /// Active manual override AINs for the current scope evaluation.
    pub cycle_override_ains: HashSet<String>,
    /// Timestamp of most recent `evaluate` call.
    last_eval_ts: Option<f64>,
    /// Application logger.
    pub(super) logger: Logger,
}

impl HumidityControlRule {
    /// Creates a new [`HumidityControlRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            boost_until: HashMap::new(),
            previous_targets: HashMap::new(),
            shielding_since: HashMap::new(),
            high_humidity_since: HashMap::new(),
            transient_latched: HashMap::new(),
            scope_states: HashMap::new(),
            recent_samples: HashMap::new(),
            cycle_override_ains: HashSet::new(),
            last_eval_ts: None,
            logger,
        }
    }

    /// Returns normalized AINs currently holding an active thermal-shielding boost.
    #[must_use]
    pub fn active_boost_ains(&self) -> HashSet<String> {
        let now = self.last_eval_ts.unwrap_or_else(now_epoch_f64);
        self.boost_until
            .iter()
            .filter(|(_, &exp)| now < exp)
            .map(|(ain, _)| normalize_ain(ain))
            .collect()
    }

    /// Restores expired legacy boost timers not managed by active v2 shielding.
    pub async fn check_expired_boosts(
        &mut self,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        now_ts: Option<f64>,
    ) {
        let now = now_ts.unwrap_or_else(now_epoch_f64);
        let expired: Vec<String> = self
            .boost_until
            .iter()
            .filter(|(ain, &exp)| now >= exp && !self.shielding_since.contains_key(*ain))
            .map(|(ain, _)| ain.clone())
            .collect();
        for ain in expired {
            self.boost_until.remove(&ain);
            if let (Some(restore_t), Some(actor)) = (
                self.previous_targets.remove(&ain),
                registry.get_device(&ain),
            ) {
                self.logger.info(&format!(
                    "Humidity boost expired for {}, restoring {restore_t}°C.",
                    actor.name
                ));
                if let Err(err) = dispatch_thermostat_target(&actor, restore_t, registry, aha).await
                {
                    self.logger.warning(&format!(
                        "Failed restoring boost target on {}: {err}",
                        actor.name
                    ));
                }
            }
        }
    }

    /// Releases active thermal shielding for `ains`, coordinating with active `Pre-Heat`.
    pub async fn release_scope_devices(
        &mut self,
        ains: &[String],
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        scheduled_base_temp: Option<f64>,
        restore_targets: bool,
    ) {
        for ain in ains {
            let actor_opt = registry.get_device(ain);
            let key = actor_opt
                .as_ref()
                .map_or_else(|| ain.clone(), |d| d.ain.clone());
            let was_guarded = self.shielding_since.remove(&key).is_some()
                || self.boost_until.remove(&key).is_some();
            let saved_base = self.previous_targets.remove(&key);
            registry.mutate_device(&key, |d| d.humidity_guard_active = false);

            let Some(actor) = actor_opt.filter(|_| was_guarded && restore_targets) else {
                continue;
            };
            if self
                .cycle_override_ains
                .contains(&normalize_ain(&actor.ain))
            {
                continue;
            }
            let preheat_t = actor
                .as_thermostat()
                .filter(|t| t.preheat_active)
                .and_then(|t| t.preheat_target_temp)
                .unwrap_or(0.0);
            let base_restore = scheduled_base_temp.filter(|&t| t > 0.0).or(saved_base);
            let restore = (base_restore.is_some() || preheat_t > 0.0)
                .then(|| base_restore.unwrap_or(0.0).max(preheat_t));
            if let Some(target) = restore.filter(|&t| t > 0.0) {
                let win_open = actor.as_thermostat().is_some_and(|t| t.window_open);
                let cur_t = actor.as_thermostat().map_or(0.0, |t| t.temperature_target);
                if win_open {
                    registry.mutate_device(&actor.ain, |d| {
                        if let Some(t) = d.as_thermostat_mut() {
                            t.temperature_target = target;
                        }
                    });
                } else if (cur_t - target).abs() >= RULE_TEMP_EPSILON {
                    if let Err(err) =
                        dispatch_thermostat_target(&actor, target, registry, aha).await
                    {
                        self.logger.warning(&format!(
                            "Failed releasing HumidityGuard on {}: {err}",
                            actor.name
                        ));
                    }
                }
            }
        }
    }

    /// Passively enriches `DIN 4108-2` telemetry when `HumidityControl` is disabled.
    pub fn enrich_passive_scope(
        &mut self,
        config: &HumidityControlConfig,
        registry: &StateRegistry,
        weather_service: Option<&WeatherService>,
        scheduled_base_temp: Option<f64>,
    ) {
        let skey = build_scope_key(config);
        self.scope_states.remove(&skey);
        self.high_humidity_since.remove(&skey);
        self.transient_latched.remove(&skey);
        let (_, temp_in, rh_in) = resolve_scope_climate(config, registry);
        let (temp_out, abs_out) = outdoor_climate(weather_service);
        let base_t = resolve_scope_base_temp(
            config,
            registry,
            &self.previous_targets,
            scheduled_base_temp,
        );
        let climate = (
            temp_in.unwrap_or(0.0),
            rh_in.unwrap_or(0.0),
            temp_out,
            abs_out,
        );
        enrich_scope_psychrometrics(config, registry, climate, base_t, false, rh_in, false);
    }

    /// Evaluates the 4-state `HumidityControl` v2 FSM for a room/group/global scope.
    pub async fn evaluate(
        &mut self,
        config: &HumidityControlConfig,
        registry: &StateRegistry,
        weather_service: Option<&WeatherService>,
        aha: &FritzAhaClient,
        db: &Database,
        params: HumidityEvalParams<'_>,
    ) {
        self.cycle_override_ains = params.manual_override_ains.clone();
        let now_ts = now_epoch_f64();
        self.last_eval_ts = Some(now_ts);
        self.check_expired_boosts(registry, aha, Some(now_ts)).await;

        let (src_ain, Some(temp_in), Some(rh_in)) = resolve_scope_climate(config, registry) else {
            return;
        };
        if config.actor_ain.is_empty() && config.sensor_ain.trim().is_empty() {
            return;
        }
        if !params.heating_allowed || config.max_temp_boost_c <= 0.0 {
            self.release_scope_devices(
                &config.actor_ain,
                registry,
                aha,
                params.scheduled_base_temp,
                params.heating_allowed,
            )
            .await;
        }
        let (temp_out, abs_out) = outdoor_climate(weather_service);
        let climate: ScopeClimate = (temp_in, rh_in, temp_out, abs_out);
        let gradients = self
            .compute_gradients(&src_ain, config, temp_in, rh_in, now_ts, db)
            .await;
        let base_t = resolve_scope_base_temp(
            config,
            registry,
            &self.previous_targets,
            params.scheduled_base_temp,
        );
        let skey = build_scope_key(config);
        let any_shielding = params.heating_allowed
            && config.max_temp_boost_c > 0.0
            && config.actor_ain.iter().any(|a| {
                registry
                    .get_device(a)
                    .is_some_and(|d| self.shielding_since.contains_key(&d.ain))
            });

        if any_shielding {
            self.handle_active_shielding(
                &skey,
                config,
                registry,
                aha,
                climate,
                base_t,
                gradients.min_delta_t_10m,
                params.scheduled_base_temp,
                now_ts,
            )
            .await;
        } else {
            self.handle_unshielded(
                &skey,
                config,
                registry,
                aha,
                climate,
                base_t,
                gradients,
                now_ts,
                params.heating_allowed,
            )
            .await;
        }
    }

    /// Builds the `HumidityControl` per-scope diagnostic decision trace JSON object.
    #[must_use]
    pub fn describe_scope_trace(
        &self,
        config: &HumidityControlConfig,
        is_active: bool,
        targets: &[String],
        registry: &StateRegistry,
        weather_service: Option<&WeatherService>,
        scheduled_base_temp: Option<f64>,
    ) -> Value {
        let boost_set = self.active_boost_ains();
        build_humidity_scope_trace(
            config,
            is_active,
            targets,
            registry,
            weather_service,
            scheduled_base_temp,
            &self.scope_states,
            &self.previous_targets,
            &boost_set,
        )
    }

    /// Records live sample, merges recent SQLite rows, and computes 15m/10m gradients.
    async fn compute_gradients(
        &mut self,
        source_ain: &str,
        config: &HumidityControlConfig,
        temp_in: f64,
        rh_in: f64,
        now_ts: f64,
        db: &Database,
    ) -> ClimateGradients {
        let key_src = if !source_ain.is_empty() {
            source_ain
        } else {
            config.actor_ain.first().map_or("", String::as_str)
        };
        let norm_key = normalize_ain(key_src);
        let retention = SAMPLE_RETENTION_SECS.max(((HUMIDITY_SUSTAIN_MINUTES + 1) as f64) * 60.0);
        let cutoff = now_ts - retention;

        let buf = self.recent_samples.entry(norm_key).or_default();
        buf.retain(|s| s.timestamp >= cutoff);
        buf.push(ClimateSample {
            timestamp: now_ts,
            temp_c: temp_in,
            rh_pct: rh_in,
        });
        let mem_copy = buf.clone();

        let mut combined = load_recent_sql_samples(source_ain, cutoff, now_ts, db).await;
        combined.extend(mem_copy);
        compute_climate_gradients(&combined, temp_in, rh_in, config.humidity_threshold, now_ts)
    }
}
