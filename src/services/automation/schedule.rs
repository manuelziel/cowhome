//! Edge-triggered 7-Day / 48-Slot `HeatingSchedule` & `PowerSchedule` rule.
//!
//! Evaluates weekday-specific time blocks in the home IANA timezone (`chrono-tz`),
//! preserves manual user overrides until the next phase boundary via midnight-continuous
//! `slot_id` signatures, and drives both radiator thermostats and smart switches.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike};
use chrono_tz::Tz;

use super::actuation::{dispatch_thermostat_target, RULE_TEMP_EPSILON};
use super::schedule_slots::{parse_day_blocks, resolve_comfort_slot_id, resolve_eco_slot_id};
use crate::core::domain_constants::{
    automation::{automation_identifiers::WEEKDAYS, sync_defaults::TARGET_HOLD_SECONDS},
    hardware::{device_capabilities, device_types},
};
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::models::automation::HeatingScheduleConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;

// =============================================================================
/// Stateful edge-triggered weekly schedule evaluator (`HeatingSchedule` / `PowerSchedule`).
#[derive(Debug)]
pub struct HeatingScheduleRule {
    /// Active phase signature (`"mon:360-480"`, `"mon:eco:480"`, etc.) per normalized AIN.
    active_slot_by_ain: HashMap<String, String>,
    /// Latched manual override flags per normalized AIN within the current phase.
    manual_override_by_ain: HashMap<String, bool>,
    /// Application logger.
    logger: Logger,
}

impl HeatingScheduleRule {
    /// Creates a new [`HeatingScheduleRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            active_slot_by_ain: HashMap::new(),
            manual_override_by_ain: HashMap::new(),
            logger,
        }
    }

    /// Clears all cached slot signatures and manual overrides so schedules re-apply immediately.
    pub fn invalidate_all(&mut self) {
        self.active_slot_by_ain.clear();
        self.manual_override_by_ain.clear();
    }

    /// Invalidates cached slot and manual override state for `ains`.
    pub fn invalidate_devices(&mut self, ains: &[String]) {
        for ain in ains {
            let norm = normalize_ain(ain);
            self.active_slot_by_ain.remove(&norm);
            self.manual_override_by_ain.remove(&norm);
        }
    }

    /// Latches a manual user override for `ains` in `slot_id` until the next phase transition.
    pub fn record_manual_override(&mut self, ains: &[String], slot_id: Option<&str>) {
        for ain in ains {
            let norm = normalize_ain(ain);
            if norm.is_empty() {
                continue;
            }
            if let Some(sid) = slot_id {
                self.active_slot_by_ain.insert(norm.clone(), sid.to_owned());
            }
            self.manual_override_by_ain.insert(norm, true);
        }
    }

    /// Returns `true` if `ain` holds an active manual override in the current schedule slot.
    #[must_use]
    pub fn is_manual_override_active(&self, ain: &str) -> bool {
        self.manual_override_by_ain
            .get(&normalize_ain(ain))
            .copied()
            .unwrap_or(false)
    }

    /// Returns all normalized AINs currently holding a manual schedule override.
    #[must_use]
    pub fn manual_override_ains(&self) -> HashSet<String> {
        self.manual_override_by_ain
            .iter()
            .filter(|(_, &active)| active)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Exposes a cloned map of `normalized_ain -> active_slot_id` for decision traces.
    #[must_use]
    pub fn active_slots_snapshot(&self) -> HashMap<String, String> {
        self.active_slot_by_ain.clone()
    }

    /// Anchors `AdaptiveStart` pre-heating devices to the current slot without dispatching eco.
    pub fn anchor_devices_slot(
        &mut self,
        ains: &[String],
        config: &HeatingScheduleConfig,
        now_dt: DateTime<Tz>,
    ) {
        if ains.is_empty() {
            return;
        }
        let (slot_id, _) = Self::resolve_active_slot(config, now_dt);
        for ain in ains {
            let norm = normalize_ain(ain);
            let prev = self
                .active_slot_by_ain
                .insert(norm.clone(), slot_id.clone());
            if prev.as_deref() != Some(slot_id.as_str()) && prev.is_some() {
                self.manual_override_by_ain.insert(norm, false);
            }
        }
    }

    /// Evaluates and dispatches edge-triggered slot transitions across `config.affected_devices`.
    pub async fn evaluate(
        &mut self,
        config: &HeatingScheduleConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        now_dt: DateTime<Tz>,
    ) {
        if config.affected_devices.is_empty() {
            return;
        }
        let (slot_id, target_temp) = Self::resolve_active_slot(config, now_dt);
        for ain in &config.affected_devices {
            let Some(dev) = registry.get_device(ain) else {
                continue;
            };
            let valid = matches!(
                dev.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP | device_types::SWITCH
            );
            if valid && dev.present {
                self.apply_slot_transition(&dev, &slot_id, target_temp, registry, aha)
                    .await;
            }
        }
    }

    /// Resolves `(slot_signature, target_temperature_c)` at local `ref_dt`.
    #[must_use]
    pub fn resolve_active_slot(
        config: &HeatingScheduleConfig,
        ref_dt: DateTime<Tz>,
    ) -> (String, f64) {
        let day_idx = ref_dt.weekday().num_days_from_monday() as usize;
        let day_key = WEEKDAYS[day_idx];
        let prev_key = WEEKDAYS[(day_idx + 6) % 7];
        #[allow(clippy::cast_possible_truncation)]
        let now_mins = (ref_dt.hour() * 60 + ref_dt.minute()) as u16;
        let blocks = parse_day_blocks(
            config.days.get(day_key).map_or(&[], Vec::as_slice),
            config.comfort_temp,
            config.days.get(prev_key).map(Vec::as_slice),
        );
        let mut prev_end = 0_u16;
        for &(start_m, end_m, temp) in &blocks {
            if start_m <= now_mins && now_mins < end_m {
                return (
                    resolve_comfort_slot_id(config, day_idx, start_m, end_m, temp),
                    temp,
                );
            }
            if end_m <= now_mins {
                prev_end = prev_end.max(end_m);
            }
        }
        (
            resolve_eco_slot_id(config, day_idx, prev_end),
            config.eco_temp,
        )
    }

    /// Finds the next upcoming comfort transition `(unix_epoch_secs, target_temp)` within `24 h`.
    #[must_use]
    pub fn get_next_comfort_transition(
        config: &HeatingScheduleConfig,
        now_dt: DateTime<Tz>,
    ) -> Option<(f64, f64)> {
        if config.days.is_empty() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let now_mins = (now_dt.hour() * 60 + now_dt.minute()) as u16;
        for day_offset in [0_i64, 1_i64] {
            let shifted = now_dt + ChronoDuration::days(day_offset);
            let day_idx = shifted.weekday().num_days_from_monday() as usize;
            let day_key = WEEKDAYS[day_idx];
            let prev_key = WEEKDAYS[(day_idx + 6) % 7];
            let blocks = parse_day_blocks(
                config.days.get(day_key).map_or(&[], Vec::as_slice),
                config.comfort_temp,
                config.days.get(prev_key).map(Vec::as_slice),
            );
            for (start_m, _, temp) in blocks {
                if temp > config.eco_temp && (day_offset > 0 || start_m > now_mins) {
                    let h = u32::from(start_m / 60);
                    let m = u32::from(start_m % 60);
                    if let Some(target_dt) = shifted
                        .with_hour(h)
                        .and_then(|d| d.with_minute(m))
                        .and_then(|d| d.with_second(0))
                        .and_then(|d| d.with_nanosecond(0))
                    {
                        return Some((target_dt.timestamp() as f64, temp));
                    }
                }
            }
        }
        None
    }

    /// Applies a single device slot check / edge-triggered transition.
    async fn apply_slot_transition(
        &mut self,
        device: &DeviceState,
        slot_id: &str,
        target_temp: f64,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        let norm = normalize_ain(&device.ain);
        let cur_target = device.as_thermostat().map_or(0.0, |t| t.temperature_target);
        if device.device_type != device_types::SWITCH
            && device.as_thermostat().is_some_and(|t| t.window_open)
        {
            if self.active_slot_by_ain.get(&norm).map(String::as_str) != Some(slot_id) {
                registry.mutate_device(&device.ain, |d| {
                    if let Some(t) = d.as_thermostat_mut() {
                        t.temperature_target = target_temp;
                    }
                });
                self.manual_override_by_ain.insert(norm, false);
            }
            return;
        }
        if self.active_slot_by_ain.get(&norm).map(String::as_str) == Some(slot_id) {
            if device.device_type == device_types::SWITCH {
                let desired = !slot_id.contains(":eco:") && target_temp > 0.0;
                if device.as_switch().is_some_and(|s| s.state != desired) {
                    self.manual_override_by_ain.insert(norm, true);
                }
            } else if cur_target > 0.0 && (cur_target - target_temp).abs() >= RULE_TEMP_EPSILON {
                self.manual_override_by_ain.insert(norm, true);
            }
            return;
        }
        self.manual_override_by_ain.insert(norm.clone(), false);
        if device.device_type == device_types::SWITCH {
            self.apply_switch_slot(device, slot_id, target_temp, registry, aha)
                .await;
            self.active_slot_by_ain.insert(norm, slot_id.to_owned());
            return;
        }
        let hw_diff = device.as_thermostat().is_some_and(|t| {
            !t.offset_control_active
                && t.hardware_target_temperature
                    .is_some_and(|hw| (hw - target_temp).abs() >= RULE_TEMP_EPSILON)
        });
        if (cur_target - target_temp).abs() >= RULE_TEMP_EPSILON || hw_diff {
            self.logger.info(&format!(
                "HeatingSchedule [{}]: Transition -> {target_temp}°C ({slot_id})",
                device.name
            ));
            if let Err(err) = dispatch_thermostat_target(device, target_temp, registry, aha).await {
                self.logger.error(&format!(
                    "Schedule transition failed for {}: {err}",
                    device.name
                ));
                return;
            }
        } else {
            let hold = now_epoch_f64() + TARGET_HOLD_SECONDS;
            registry.mutate_device(&device.ain, |d| {
                if let Some(t) = d.as_thermostat_mut() {
                    t.target_hold_until = hold;
                }
            });
        }
        self.active_slot_by_ain.insert(norm, slot_id.to_owned());
    }

    /// Applies an ON/OFF transition to a switchable smart outlet.
    async fn apply_switch_slot(
        &self,
        device: &DeviceState,
        slot_id: &str,
        target_temp: f64,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        if !device.capabilities.is_empty()
            && !device
                .capabilities
                .iter()
                .any(|c| c == device_capabilities::SWITCH)
        {
            return;
        }
        let desired = !slot_id.contains(":eco:") && target_temp > 0.0;
        let current = device.as_switch().is_some_and(|s| s.state);
        if current != desired {
            let label = if desired { "ON" } else { "OFF" };
            self.logger.info(&format!(
                "PowerSchedule [{}]: Transition -> {label} ({slot_id})",
                device.name
            ));
            if let Err(err) = aha.set_switch(&device.ain, desired).await {
                self.logger
                    .error(&format!("PowerSchedule failed for {}: {err}", device.name));
                return;
            }
            registry.mutate_device(&device.ain, |d| {
                if let Some(sw) = d.as_switch_mut() {
                    sw.state = desired;
                }
            });
        }
    }
}
