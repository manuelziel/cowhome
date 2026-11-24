//! `AdaptiveStart` (`Pre-Heat`) optimum start control per `DIN EN 12098-1`.
//!
//! Computes weather-compensated pre-heating lead times (`<= 180 min`) from learned
//! room ramp rates (`effective_heating_rate_min_per_c`), rehydrates active latches across
//! restarts, detects physical dial aborts, coordinates seamlessly with `HumidityControl`,
//! and publishes live `preheat_active` / `preheat_target_temp` / `preheat_target_time`.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;
use serde_json::Value;

use super::actuation::{RULE_TEMP_EPSILON, dispatch_thermostat_target, preheat_paused_reasons};
use super::adaptive_helpers::{
    build_adaptive_scope_trace, resolve_current_room_temp, resolve_target_devices,
    set_twin_preheat_state,
};
use crate::core::domain_constants::{
    analytics::efficiency_defaults::SECONDS_PER_MINUTE as SECS_PER_MIN,
    automation::automation_defaults::{PREHEAT_MAX_MINUTES, PREHEAT_MIN_DELTA_C},
};
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::automation::AdaptiveStartConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;
use crate::services::weather::WeatherService;

// =============================================================================
/// Stateful `DIN EN 12098-1` predictive pre-heating rule (`AdaptiveStart`).
#[derive(Debug)]
pub struct AdaptiveStartRule {
    /// Latched comfort target epochs (`unix_secs`) per normalized AIN.
    latched_epochs: HashMap<String, i64>,
    /// Dispatched comfort target epochs per normalized AIN (prevents duplicate sends).
    dispatched_epochs: HashMap<String, i64>,
    /// User-aborted comfort target epochs per normalized AIN.
    aborted_epochs: HashMap<String, i64>,
    /// Application logger.
    logger: Logger,
}

impl AdaptiveStartRule {
    /// Creates a new [`AdaptiveStartRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            latched_epochs: HashMap::new(),
            dispatched_epochs: HashMap::new(),
            aborted_epochs: HashMap::new(),
            logger,
        }
    }

    /// Clears latched pre-heat state and resets live Digital Twin flags for `ains`.
    pub fn clear_devices(
        &mut self,
        ains: &[String],
        keep_aborted: bool,
        registry: Option<&StateRegistry>,
    ) {
        for ain in ains {
            let norm = normalize_ain(ain);
            self.latched_epochs.remove(&norm);
            self.dispatched_epochs.remove(&norm);
            if !keep_aborted {
                self.aborted_epochs.remove(&norm);
            }
            if let Some(reg) = registry {
                set_twin_preheat_state(reg, ain, false, None, None, !keep_aborted);
            }
        }
    }

    /// Aborts active pre-heating when the user manually adjusts `ains`.
    pub fn record_manual_override(&mut self, ains: &[String], registry: Option<&StateRegistry>) {
        for ain in ains {
            let norm = normalize_ain(ain);
            let popped = self.latched_epochs.remove(&norm);
            if let Some(epoch) = popped {
                self.aborted_epochs.insert(norm.clone(), epoch);
            }
            self.dispatched_epochs.remove(&norm);
            if let Some(reg) = registry {
                reg.mutate_device(ain, |dev| {
                    if let Some(t) = dev.as_thermostat_mut() {
                        t.preheat_active = false;
                        t.preheat_target_temp = None;
                        t.preheat_target_time = None;
                        if popped.is_some() {
                            t.preheat_paused_reason =
                                Some(preheat_paused_reasons::MANUAL_OVERRIDE.to_owned());
                        }
                    }
                });
            }
        }
    }

    /// Checks whether upcoming `target_epoch` was manually aborted on `ain`.
    #[must_use]
    pub fn is_epoch_aborted(&self, ain: &str, target_epoch: Option<f64>, now_ts: f64) -> bool {
        let Some(t_ep) = target_epoch else {
            return false;
        };
        #[allow(clippy::cast_possible_truncation)]
        let key = t_ep as i64;
        now_ts < t_ep && self.aborted_epochs.get(&normalize_ain(ain)) == Some(&key)
    }

    /// Returns `true` if `ain` is currently latched inside an active pre-heat window.
    #[must_use]
    pub fn is_device_preheating(&self, ain: &str, now_ts: f64) -> bool {
        let norm = normalize_ain(ain);
        let Some(&epoch) = self.latched_epochs.get(&norm) else {
            return false;
        };
        self.aborted_epochs.get(&norm) != Some(&epoch) && now_ts < (epoch as f64)
    }

    /// Returns sorted normalized AINs currently in an active pre-heat session.
    #[must_use]
    pub fn active_preheating_ains(&self, now_ts: f64) -> Vec<String> {
        let mut out: Vec<String> = self
            .latched_epochs
            .keys()
            .filter(|ain| self.is_device_preheating(ain, now_ts))
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Evaluates `AdaptiveStart` pre-heating across `config` targets.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &mut self,
        config: &AdaptiveStartConfig,
        is_active: bool,
        registry: &StateRegistry,
        db: &Database,
        weather_service: Option<&WeatherService>,
        aha: &FritzAhaClient,
        schedule_transition: Option<(f64, f64)>,
        tz: Tz,
    ) {
        let now_ts = now_epoch_f64();
        self.prune_expired_epochs(now_ts);
        let all_devs = resolve_target_devices(config, registry);
        if !is_active {
            let ains: Vec<String> = all_devs.iter().map(|d| d.ain.clone()).collect();
            self.clear_devices(&ains, false, Some(registry));
            return;
        }
        let win_open: Vec<String> = all_devs
            .iter()
            .filter(|d| d.as_thermostat().is_some_and(|t| t.window_open))
            .map(|d| d.ain.clone())
            .collect();
        if !win_open.is_empty() {
            self.clear_devices(&win_open, true, Some(registry));
        }
        let devices: Vec<DeviceState> = all_devs
            .into_iter()
            .filter(|d| !d.as_thermostat().is_some_and(|t| t.window_open))
            .collect();
        let Some((target_epoch, target_temp)) = schedule_transition else {
            for dev in &devices {
                set_twin_preheat_state(registry, &dev.ain, false, None, None, false);
            }
            return;
        };
        let Some(cur_temp) = resolve_current_room_temp(&devices) else {
            for dev in &devices {
                set_twin_preheat_state(registry, &dev.ain, false, None, None, false);
            }
            return;
        };

        let live_out = weather_service
            .and_then(WeatherService::get_current_weather)
            .and_then(|c| c.temperature);
        if self
            .check_and_latch_window(
                cur_temp,
                target_epoch,
                target_temp,
                &devices,
                db,
                live_out,
                now_ts,
            )
            .await
        {
            #[allow(clippy::cast_possible_truncation)]
            let target_time_str = tz
                .timestamp_opt(target_epoch as i64, 0)
                .single()
                .map_or_else(
                    || String::from("00:00"),
                    |dt| dt.format("%H:%M").to_string(),
                );
            self.trigger_preheat(
                &devices,
                target_epoch,
                target_temp,
                &target_time_str,
                registry,
                aha,
            )
            .await;
        } else {
            for dev in &devices {
                set_twin_preheat_state(registry, &dev.ain, false, None, None, false);
            }
        }
    }

    /// Builds the `AdaptiveStart` per-scope diagnostic decision trace JSON object.
    #[allow(clippy::too_many_arguments)]
    pub async fn describe_scope_trace(
        &self,
        config: &AdaptiveStartConfig,
        is_active: bool,
        registry: &StateRegistry,
        db: &Database,
        schedule_transition: Option<(f64, f64)>,
        now_dt: DateTime<Tz>,
        weather_service: Option<&WeatherService>,
    ) -> Value {
        let now_ts = now_epoch_f64();
        let devices = resolve_target_devices(config, registry);
        let mut preheating_ains: Vec<String> = devices
            .iter()
            .filter(|d| is_active && self.is_device_preheating(&d.ain, now_ts))
            .map(|d| normalize_ain(&d.ain))
            .collect();
        preheating_ains.sort();
        build_adaptive_scope_trace(
            config,
            is_active,
            registry,
            db,
            schedule_transition,
            now_dt,
            weather_service,
            preheating_ains,
        )
        .await
    }

    /// Removes expired transition epochs from internal tracking maps.
    fn prune_expired_epochs(&mut self, now_ts: f64) {
        self.latched_epochs
            .retain(|_, &mut ep| (ep as f64) > now_ts);
        self.dispatched_epochs
            .retain(|_, &mut ep| (ep as f64) > now_ts);
        self.aborted_epochs
            .retain(|_, &mut ep| (ep as f64) > now_ts);
    }

    /// Checks whether `now_ts` falls inside the `DIN EN 12098-1` lead window and latches it.
    #[allow(clippy::too_many_arguments)]
    async fn check_and_latch_window(
        &mut self,
        cur_temp: f64,
        target_epoch: f64,
        target_temp: f64,
        devices: &[DeviceState],
        db: &Database,
        live_out: Option<f64>,
        now_ts: f64,
    ) -> bool {
        #[allow(clippy::cast_possible_truncation)]
        let epoch_key = target_epoch as i64;
        if now_ts < target_epoch {
            for dev in devices {
                let norm = normalize_ain(&dev.ain);
                if !self.latched_epochs.contains_key(&norm)
                    && dev.as_thermostat().is_some_and(|t| {
                        t.preheat_active
                            && t.preheat_target_temp
                                .is_some_and(|pt| (pt - target_temp).abs() < RULE_TEMP_EPSILON)
                    })
                {
                    self.latched_epochs.insert(norm.clone(), epoch_key);
                    self.dispatched_epochs.insert(norm, epoch_key);
                }
            }
        }
        let already_latched = now_ts < target_epoch
            && devices
                .iter()
                .any(|d| self.latched_epochs.get(&normalize_ain(&d.ain)) == Some(&epoch_key));

        let preheat_mins = if already_latched {
            0.0
        } else {
            let deficit = target_temp - cur_temp;
            if deficit < PREHEAT_MIN_DELTA_C {
                0.0
            } else {
                let ains: Vec<String> = devices.iter().map(|d| d.ain.clone()).collect();
                let m = db
                    .run_blocking(move |d| {
                        Ok(d.resolve_scope_heating_metrics(&ains, live_out, None, None))
                    })
                    .await
                    .unwrap_or_else(|_| {
                        db.resolve_scope_heating_metrics(&[], live_out, None, None)
                    });
                (deficit * m.effective_heating_rate_min_per_c)
                    .round()
                    .min(PREHEAT_MAX_MINUTES as f64)
            }
        };

        let in_window = already_latched
            || (preheat_mins > 0.0
                && now_ts >= (target_epoch - preheat_mins * SECS_PER_MIN)
                && now_ts < target_epoch);
        if in_window {
            for dev in devices {
                let norm = normalize_ain(&dev.ain);
                let win_open = dev.as_thermostat().is_some_and(|t| t.window_open);
                if !win_open && self.aborted_epochs.get(&norm) != Some(&epoch_key) {
                    self.latched_epochs.insert(norm, epoch_key);
                }
            }
        }
        in_window
    }

    /// Triggers synchronous pre-heating across `devices` unless manually lowered via dial.
    async fn trigger_preheat(
        &mut self,
        devices: &[DeviceState],
        target_epoch: f64,
        target_temp: f64,
        target_time_str: &str,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let epoch_key = target_epoch as i64;
        let lowered = devices.iter().find_map(|d| {
            let t = d.as_thermostat()?;
            let norm = normalize_ain(&d.ain);
            (!t.window_open
                && self.dispatched_epochs.get(&norm) == Some(&epoch_key)
                && t.temperature_target > 0.0
                && t.temperature_target < target_temp - RULE_TEMP_EPSILON)
                .then_some(t.temperature_target)
        });

        if let Some(abort_temp) = lowered {
            for dev in devices {
                let norm = normalize_ain(&dev.ain);
                self.aborted_epochs.insert(norm.clone(), epoch_key);
                self.latched_epochs.remove(&norm);
                set_twin_preheat_state(registry, &dev.ain, false, None, None, false);
                let should_revert = dev.as_thermostat().is_some_and(|t| {
                    !t.window_open && (t.temperature_target - target_temp).abs() < RULE_TEMP_EPSILON
                });
                if should_revert
                    && let Err(err) =
                        dispatch_thermostat_target(dev, abort_temp, registry, aha).await
                {
                    self.logger.warning(&format!(
                        "Pre-heat abort revert failed for {}: {err}",
                        dev.name
                    ));
                }
            }
            return;
        }

        for dev in devices {
            let norm = normalize_ain(&dev.ain);
            let win_open = dev.as_thermostat().is_some_and(|t| t.window_open);
            if win_open || self.aborted_epochs.get(&norm) == Some(&epoch_key) {
                set_twin_preheat_state(registry, &dev.ain, false, None, None, false);
                continue;
            }
            let cur_target = dev.as_thermostat().map_or(0.0, |t| t.temperature_target);
            let already_sent = self.dispatched_epochs.insert(norm, epoch_key) == Some(epoch_key);
            set_twin_preheat_state(
                registry,
                &dev.ain,
                true,
                Some(target_temp),
                Some(target_time_str),
                true,
            );
            if !already_sent && cur_target < target_temp - RULE_TEMP_EPSILON {
                self.logger.info(&format!(
                    "AdaptiveStart: Pre-heating {} to {target_temp}°C (for {target_time_str}).",
                    dev.name
                ));
                if let Err(err) = dispatch_thermostat_target(dev, target_temp, registry, aha).await
                {
                    self.logger.error(&format!(
                        "AdaptiveStart failed setting target on {}: {err}",
                        dev.name
                    ));
                }
            }
        }
    }
}
