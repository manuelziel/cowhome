//! Periodic `TR-064` router/presence polling, house analytics refresh, & `00:00` auto-prune.
//!
//! Separated from [`super::poller`] so cyclic worker orchestration and housekeeping
//! helpers each remain concise (`<= 450` lines).

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Timelike};
use serde_json::Value;

use super::poller::{FritzPollingWorker, WorkerCycleState};
use super::state_builder::TEMP_CHANGE_THRESHOLD;
use crate::core::constants::service_defaults::RATE_LIMIT_CLEANUP_SECS;
use crate::core::domain_constants::{
    analytics::history_defaults::DEFAULT_RETENTION_DAYS,
    automation::automation_identifiers::{PRESENCE_PROFILING, SCOPE_GLOBAL},
    hardware::{
        special_devices::{ROUTER_AIN, ROUTER_NAME},
        tr064_defaults::UNKNOWN_META,
    },
};
use crate::core::error::AppResult;
use crate::core::time_util::{normalize_ain, resolve_home_tz};
use crate::db::Database;
use crate::db::efficiency::AnalyticsContext;
use crate::models::device::DeviceState;

impl FritzPollingWorker {
    /// Polls `ROUTER_01` availability and model metadata via TR-064.
    pub(super) async fn poll_router_device(&self) -> DeviceState {
        let custom_name = self
            .db
            .run_blocking(|db| db.get_custom_name(ROUTER_AIN))
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| ROUTER_NAME.to_owned());
        let dev_info = self.tr064.get_device_info().await;
        let model_opt = dev_info
            .as_ref()
            .map(|i| i.model.clone())
            .filter(|m| m != UNKNOWN_META);
        let present = self.tr064.get_system_status().await.is_ok() || dev_info.is_some();
        let mut router = DeviceState::new_router(present, model_opt);
        router.name = custom_name;
        router
    }

    /// Polls active TR-064 Wi-Fi/LAN hosts if any `PresenceProfiling` rule has tracked MACs.
    pub(super) async fn poll_configured_presence_macs(&self) -> Vec<(String, bool)> {
        let tracked_macs = self
            .db
            .run_blocking(collect_tracked_presence_macs)
            .await
            .unwrap_or_default();
        if tracked_macs.is_empty() {
            return Vec::new();
        }
        let Ok(hosts) = self.tr064.get_connected_hosts().await else {
            return Vec::new();
        };
        let active_set: HashSet<String> = hosts
            .into_iter()
            .filter(|h| h.status)
            .map(|h| h.mac.trim().to_ascii_lowercase())
            .collect();
        tracked_macs
            .into_iter()
            .map(|mac| {
                let is_home = active_set.contains(&mac);
                (mac, is_home)
            })
            .collect()
    }

    /// Recomputes house-wide 24h `heating_efficiency` (+ 5-day forecast) and `energy_summary`.
    pub(super) async fn refresh_house_analytics(&self) {
        let (daily_fc, live_out, tz_name) = match &self.weather_service {
            Some(ws) => {
                let snap = ws.get_snapshot();
                let out_t = snap.current.as_ref().and_then(|c| c.temperature);
                (snap.to_forecast_inputs(), out_t, snap.timezone)
            }
            None => (Vec::new(), None, self.db.default_timezone()),
        };
        let live_targets = self.registry.get_live_thermostat_targets();
        let live_switches = self.registry.get_live_switch_states();
        let res = self
            .db
            .run_blocking(move |db| {
                let ctx = AnalyticsContext {
                    since: None,
                    range_str: None,
                    daily_forecast: &daily_fc,
                    live_targets: Some(&live_targets),
                    live_outdoor_temp: live_out,
                };
                let eff = db.get_house_heating_efficiency(&ctx);
                let energy =
                    db.get_house_energy_summary(Some(&tz_name), None, Some(&live_switches));
                Ok((eff, energy))
            })
            .await;
        if let Ok((eff, energy)) = res {
            self.registry.set_heating_efficiency(eff);
            self.registry.set_energy_summary(energy);
        }
    }

    /// Emits a warning log when an internal FritzBox hardware offset (`!= 0.0 °C`) is detected.
    pub(super) fn warn_on_fritz_offset(&self, state: &DeviceState) {
        let Some(t) = state.as_thermostat() else {
            return;
        };
        let norm = normalize_ain(&state.ain);
        let Ok(mut guard) = self.cycle_state.lock() else {
            return;
        };
        if t.fritz_temperature_offset.abs() >= TEMP_CHANGE_THRESHOLD {
            if guard.warned_fritz_offsets.insert(norm) {
                self.logger.warning(&format!(
                    "Thermostat '{}' ({}) reports active internal FritzBox offset ({:+.1}°C); \
                     reset FritzBox offset to 0.0°C so CowHome remains Single Source of Calibration.",
                    state.name, state.ain, t.fritz_temperature_offset
                ));
            }
        } else {
            guard.warned_fritz_offsets.remove(&norm);
        }
    }

    /// Periodically purges expired sliding-window entries from `RateLimiter`.
    pub(super) fn cleanup_rate_limiter_if_due(&self, now_mono: Instant, window_secs: u64) {
        let Some(ref rl) = self.rate_limiter else {
            return;
        };
        let should_run = self.cycle_state.lock().is_ok_and(|mut g| {
            let due = g.last_rl_cleanup.is_none_or(|last| {
                now_mono.saturating_duration_since(last)
                    >= Duration::from_secs(RATE_LIMIT_CLEANUP_SECS)
            });
            if due {
                g.last_rl_cleanup = Some(now_mono);
            }
            due
        });
        if should_run {
            rl.cleanup_expired(Some(window_secs));
        }
    }

    /// Runs daily `00:00` local-time history pruning and passive `WAL` checkpointing.
    pub(super) async fn prune_history_if_due(&self, now_ts: f64) {
        let tz_name = self
            .weather_service
            .as_ref()
            .map_or_else(|| self.db.default_timezone(), |ws| ws.get_timezone_name());
        let Some(today) = pending_prune_date(now_ts, &tz_name, &self.cycle_state) else {
            return;
        };
        let res = self
            .db
            .run_blocking(|db| {
                let removed = db.prune_history(DEFAULT_RETENTION_DAYS)?;
                db.wal_checkpoint();
                Ok(removed)
            })
            .await;
        match res {
            Ok(removed) => {
                if let Ok(mut guard) = self.cycle_state.lock() {
                    guard.last_prune_date = Some(today.clone());
                }
                self.logger.info(&format!(
                    "Daily Auto-Prune ({today}): removed {removed} expired history rows."
                ));
            }
            Err(err) => self
                .logger
                .warning(&format!("Daily Auto-Prune failed: {err}")),
        }
    }
}

// =============================================================================
/// Collects all unique lowercased MAC addresses configured across active `PresenceProfiling` scopes.
fn collect_tracked_presence_macs(db: Database) -> AppResult<Vec<String>> {
    let mut macs = HashSet::new();
    let mut extract = |val: &Value| {
        let Some(feat) = val.get(PRESENCE_PROFILING) else {
            return;
        };
        if !feat
            .get("is_active")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return;
        }
        let cfg_obj = feat.get("config");
        let arr_opt = cfg_obj
            .and_then(|c| c.get("tracked_mac_addresses"))
            .or_else(|| cfg_obj.and_then(|c| c.get("mac_addresses")))
            .and_then(Value::as_array);
        if let Some(arr) = arr_opt {
            for mac in arr.iter().filter_map(Value::as_str) {
                let clean = mac.trim().to_ascii_lowercase();
                if !clean.is_empty() {
                    macs.insert(clean);
                }
            }
        }
    };
    if let Ok(glob) = db.get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL) {
        extract(&glob);
    }
    if let Ok(groups) = db.get_all_software_groups() {
        for g in &groups {
            if let Some(auto) = g.get("automation") {
                extract(auto);
            }
        }
    }
    if let Ok(devs) = db.get_all_device_scope_configs() {
        for auto in devs.values() {
            extract(auto);
        }
    }
    Ok(macs.into_iter().collect())
}

// =============================================================================
/// Initializes `last_prune_date` so daytime restarts (`> 00:00`) wait until the next midnight.
#[must_use]
pub(super) fn init_prune_date_guard(now_ts: f64, tz_name: &str) -> Option<String> {
    let tz = resolve_home_tz(Some(tz_name));
    let dt = tz.timestamp_opt(now_ts.floor() as i64, 0).single()?;
    ((dt.hour(), dt.minute()) > (0, 0)).then(|| dt.format("%Y-%m-%d").to_string())
}

// =============================================================================
/// Returns `Some("YYYY-MM-DD")` when `today` has not yet been pruned successfully.
fn pending_prune_date(
    now_ts: f64,
    tz_name: &str,
    state: &Mutex<WorkerCycleState>,
) -> Option<String> {
    let tz = resolve_home_tz(Some(tz_name));
    let dt = tz.timestamp_opt(now_ts.floor() as i64, 0).single()?;
    let today = dt.format("%Y-%m-%d").to_string();
    let guard = state.lock().ok()?;
    if guard.last_prune_date.as_deref() == Some(today.as_str()) {
        return None;
    }
    Some(today)
}
