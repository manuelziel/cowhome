//! `PresenceProfiling` (`Auto-Away`) rule via TR-064 smartphone Wi-Fi host tracking.
//!
//! Queries active LAN/WLAN MAC addresses once per `RulesEngine` cycle, persists
//! deduplicated `presence_history` pings, and lowers scope thermostats after all
//! tracked MACs have been absent for `timeout_minutes` (respecting `HumidityGuard`).

use std::collections::{HashMap, HashSet};

use super::actuation::{dispatch_thermostat_target, RULE_TEMP_EPSILON};
use crate::core::domain_constants::{
    automation::automation_defaults::FROST_PROTECTION_TEMP,
    hardware::{device_types, fritz_api::MAX_THERMOSTAT_TEMP},
};
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::automation::PresenceProfilingConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client};

// =============================================================================
/// `PresenceProfiling` (`Auto-Away`) rule via TR-064 smartphone Wi-Fi host tracking.
#[derive(Debug)]
pub struct PresenceProfilingRule {
    /// Normalized AINs currently in away setback.
    pub last_setback_ains: HashSet<String>,
    /// Pre-setback target temperatures per normalized AIN.
    pre_setback_targets: HashMap<String, f64>,
    /// Scheduled base temperature at which setback was last applied per normalized AIN.
    setback_applied_base: HashMap<String, Option<f64>>,
    /// True while running inside a `RulesEngine` multi-scope cycle.
    in_engine_cycle: bool,
    /// Per-cycle cache of active lowercased router MACs.
    cycle_active_macs: Option<HashSet<String>>,
    /// Per-cycle deduplication set of already recorded MAC presence pings.
    cycle_pinged_macs: HashSet<String>,
    /// Application logger.
    logger: Logger,
}

impl PresenceProfilingRule {
    /// Creates a new [`PresenceProfilingRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            last_setback_ains: HashSet::new(),
            pre_setback_targets: HashMap::new(),
            setback_applied_base: HashMap::new(),
            in_engine_cycle: false,
            cycle_active_macs: None,
            cycle_pinged_macs: HashSet::new(),
            logger,
        }
    }

    /// Initializes per-cycle TR-064 and `presence_history` deduplication caches.
    pub fn begin_cycle(&mut self) {
        self.last_setback_ains.clear();
        self.in_engine_cycle = true;
        self.cycle_active_macs = None;
        self.cycle_pinged_macs.clear();
    }

    /// Clears per-cycle caches after `RulesEngine` finishes all scopes.
    pub fn end_cycle(&mut self) {
        self.in_engine_cycle = false;
        self.cycle_active_macs = None;
        self.cycle_pinged_macs.clear();
    }

    /// Evaluates Wi-Fi presence and applies/restores away setback across `config.affected_devices`.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &mut self,
        config: &PresenceProfilingConfig,
        is_active: bool,
        registry: &StateRegistry,
        db: &Database,
        aha: &FritzAhaClient,
        tr064: &FritzTr064Client,
        scheduled_base_temp: Option<f64>,
    ) {
        if !self.in_engine_cycle {
            self.last_setback_ains.clear();
        }
        if config.tracked_mac_addresses.is_empty() {
            return;
        }
        let active_macs = self.resolve_connected_macs(tr064).await;
        let anyone_home = self
            .record_presence_pings(&config.tracked_mac_addresses, &active_macs, db)
            .await;

        if is_active && !config.learning_mode_only && !anyone_home {
            if has_timeout_expired(&config.tracked_mac_addresses, config.timeout_minutes, db).await
            {
                self.apply_away_setback(config, registry, aha, scheduled_base_temp)
                    .await;
            }
        } else if anyone_home {
            self.restore_from_setback(config, registry, aha, scheduled_base_temp)
                .await;
        }
    }

    /// Resolves active router MACs once per cycle via TR-064.
    async fn resolve_connected_macs(&mut self, tr064: &FritzTr064Client) -> HashSet<String> {
        if self.in_engine_cycle {
            if let Some(ref cached) = self.cycle_active_macs {
                return cached.clone();
            }
        }
        let fetched = match tr064.get_connected_hosts().await {
            Ok(hosts) => hosts
                .into_iter()
                .map(|h| h.mac.trim().to_ascii_lowercase())
                .filter(|m| !m.is_empty())
                .collect(),
            Err(err) => {
                self.logger
                    .warning(&format!("Failed querying TR-064 hosts: {err}"));
                HashSet::new()
            }
        };
        if self.in_engine_cycle {
            self.cycle_active_macs = Some(fetched.clone());
        }
        fetched
    }

    /// Persists unrecorded MAC pings in SQLite and returns `true` if any tracked MAC is online.
    async fn record_presence_pings(
        &mut self,
        tracked: &[String],
        active_macs: &HashSet<String>,
        db: &Database,
    ) -> bool {
        let mut anyone_home = false;
        let mut to_insert: Vec<(String, bool)> = Vec::new();
        for mac in tracked {
            let norm = mac.trim().to_ascii_lowercase();
            if norm.is_empty() {
                continue;
            }
            let home = active_macs.contains(&norm);
            anyone_home = anyone_home || home;
            if !self.in_engine_cycle || self.cycle_pinged_macs.insert(norm.clone()) {
                to_insert.push((norm, home));
            }
        }
        if !to_insert.is_empty() {
            let now_ts = now_epoch_f64();
            if let Err(err) = db
                .run_blocking(move |d| {
                    d.with_tx(|tx| {
                        for (mac, home) in &to_insert {
                            d.insert_presence_ping_on(tx, mac, *home, Some(now_ts))?;
                        }
                        Ok(())
                    })
                })
                .await
            {
                self.logger
                    .warning(&format!("Failed recording presence history pings: {err}"));
            }
        }
        anyone_home
    }

    /// Lowers target temperature on scope thermostats when all tracked MACs are away.
    async fn apply_away_setback(
        &mut self,
        config: &PresenceProfilingConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        scheduled_base: Option<f64>,
    ) {
        for dev in resolve_presence_candidates(config, registry) {
            let norm = normalize_ain(&dev.ain);
            self.last_setback_ains.insert(norm.clone());
            let cur_t = dev.as_thermostat().map_or(0.0, |t| t.temperature_target);
            let saved = *self
                .pre_setback_targets
                .entry(norm.clone())
                .or_insert(cur_t);
            if dev.humidity_guard_active {
                self.setback_applied_base.remove(&norm);
                continue;
            }
            if self.setback_applied_base.get(&norm) == Some(&scheduled_base) {
                continue;
            }
            self.setback_applied_base.insert(norm, scheduled_base);
            let base = scheduled_base.unwrap_or(saved);
            let raw_setback = if config.away_temp_offset >= 5.0 {
                config.away_temp_offset
            } else {
                base + config.away_temp_offset
            };
            let setback = raw_setback.clamp(FROST_PROTECTION_TEMP, MAX_THERMOSTAT_TEMP);
            let should_send = if scheduled_base.is_some() {
                (cur_t - setback).abs() >= RULE_TEMP_EPSILON
            } else {
                cur_t > setback
            };
            if should_send {
                self.logger
                    .info(&format!("Presence setback on {} to {setback}°C.", dev.name));
                if let Err(err) = dispatch_thermostat_target(&dev, setback, registry, aha).await {
                    self.logger.warning(&format!(
                        "Failed dispatching presence setback on {}: {err}",
                        dev.name
                    ));
                }
            }
        }
    }

    /// Restores pre-setback setpoints when a resident reconnects to Wi-Fi.
    async fn restore_from_setback(
        &mut self,
        config: &PresenceProfilingConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        scheduled_base: Option<f64>,
    ) {
        for dev in resolve_presence_candidates(config, registry) {
            let norm = normalize_ain(&dev.ain);
            self.setback_applied_base.remove(&norm);
            let restore = self.pre_setback_targets.remove(&norm);
            if !dev.humidity_guard_active && scheduled_base.is_none() {
                if let Some(temp) = restore.filter(|&t| t > 0.0) {
                    self.logger
                        .info(&format!("Presence restored on {} -> {temp}°C.", dev.name));
                    if let Err(err) = dispatch_thermostat_target(&dev, temp, registry, aha).await {
                        self.logger.warning(&format!(
                            "Failed restoring presence target on {}: {err}",
                            dev.name
                        ));
                    }
                }
            }
        }
    }
}

// =============================================================================
/// Returns `true` when every tracked MAC has been absent for at least `timeout_minutes`.
async fn has_timeout_expired(tracked: &[String], timeout_minutes: u64, db: &Database) -> bool {
    let macs = tracked.to_vec();
    let now = now_epoch_f64();
    let threshold = (timeout_minutes as f64) * 60.0;
    db.run_blocking(move |d| {
        for mac in &macs {
            if let Ok(Some(last_seen)) = d.get_last_presence_seen(mac) {
                if (now - last_seen) < threshold {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    })
    .await
    .unwrap_or(false)
}

// =============================================================================
/// Resolves target thermostat [`DeviceState`] entries for `PresenceProfiling`.
fn resolve_presence_candidates(
    config: &PresenceProfilingConfig,
    registry: &StateRegistry,
) -> Vec<DeviceState> {
    let raw: Vec<DeviceState> = if config.affected_devices.is_empty() {
        registry.get_all_devices()
    } else {
        config
            .affected_devices
            .iter()
            .filter_map(|a| registry.get_device(a))
            .collect()
    };
    raw.into_iter()
        .filter(|d| {
            matches!(
                d.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            )
        })
        .collect()
}
