//! Cyclic FritzBox Smart Home (`AHA`) and Router (`TR-064`) background polling worker.
//!
//! Coordinates the periodic polling loop, physical radiator dial auto-sync across
//! software groups, Digital Twin updates, and single-transaction SQLite batch commits.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::sync::watch;

use super::maintenance::init_prune_date_guard;
use super::state_builder::{
    apply_software_group_authority, build_device_state, build_software_group_ain_map,
    resolve_display_name, TEMP_CHANGE_THRESHOLD,
};
use super::sync_logic::{preserve_enriched_twin_state, resolve_effective_user_target};
use crate::core::constants::service_defaults::{
    DEFAULT_FRITZBOX_POLL_SECS, DEFAULT_RATE_LIMIT_WINDOW_SECS, WORKER_MIN_SLEEP_SECS,
    WORKER_STARTUP_DELAY_SECS,
};
use crate::core::domain_constants::{
    automation::sync_defaults::TARGET_HOLD_SECONDS,
    hardware::{device_types, fritz_bitmask, special_devices::ROUTER_AIN},
};
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client, RawAhaDevice};
use crate::services::rate_limiter::RateLimiter;
use crate::services::weather::WeatherService;

// =============================================================================
/// Mutable inter-cycle tracking state owned by [`FritzPollingWorker`].
#[derive(Debug, Default)]
pub(super) struct WorkerCycleState {
    /// Previous user target temperatures per normalized AIN for dial-turn group sync.
    pub last_target_temps: HashMap<String, f64>,
    /// Normalized AINs that have already logged an active FritzBox hardware offset warning.
    pub warned_fritz_offsets: HashSet<String>,
    /// Local `"YYYY-MM-DD"` date string of the most recent `00:00` auto-prune run.
    pub last_prune_date: Option<String>,
    /// Monotonic timestamp of the most recent rate-limiter cleanup.
    pub last_rl_cleanup: Option<Instant>,
}

// =============================================================================
/// Dependency bundle passed when constructing [`FritzPollingWorker`].
pub struct PollingWorkerDeps {
    /// Initial FritzBox poll interval in seconds.
    pub poll_interval_secs: u64,
    /// Initial rate-limiter window in seconds.
    pub rate_limit_window_secs: u64,
    /// Shared Digital Twin registry.
    pub registry: Arc<StateRegistry>,
    /// Pooled SQLite `WAL` database.
    pub db: Database,
    /// FritzBox AHA client.
    pub aha: Arc<FritzAhaClient>,
    /// FritzBox TR-064 client.
    pub tr064: Arc<FritzTr064Client>,
    /// Optional rate limiter for periodic expired-key cleanup.
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// Optional weather service for psychrometrics & 5-day heating forecast.
    pub weather_service: Option<Arc<WeatherService>>,
    /// Application logger.
    pub logger: Logger,
}

// =============================================================================
/// Async background FritzBox polling worker shared via `Arc<FritzPollingWorker>`.
#[derive(Debug)]
pub struct FritzPollingWorker {
    /// Hot-reloadable `(poll_interval_secs, rate_limit_window_secs)`.
    timing: RwLock<(u64, u64)>,
    /// Digital Twin state registry.
    pub(super) registry: Arc<StateRegistry>,
    /// SQLite database.
    pub(super) db: Database,
    /// FritzBox AHA client.
    aha: Arc<FritzAhaClient>,
    /// FritzBox TR-064 client.
    pub(super) tr064: Arc<FritzTr064Client>,
    /// Optional HTTP rate limiter.
    pub(super) rate_limiter: Option<Arc<RateLimiter>>,
    /// Optional Open-Meteo weather service.
    pub(super) weather_service: Option<Arc<WeatherService>>,
    /// Inter-cycle dial-sync, warning, and prune state.
    pub(super) cycle_state: Mutex<WorkerCycleState>,
    /// Application logger.
    pub(super) logger: Logger,
}

impl FritzPollingWorker {
    /// Constructs a new [`FritzPollingWorker`] initialized from `deps`.
    #[must_use]
    pub fn new(deps: PollingWorkerDeps) -> Self {
        let tz_name = deps
            .weather_service
            .as_ref()
            .map_or_else(|| deps.db.default_timezone(), |ws| ws.get_timezone_name());
        let init_prune = init_prune_date_guard(now_epoch_f64(), &tz_name);
        Self {
            timing: RwLock::new((deps.poll_interval_secs.max(5), deps.rate_limit_window_secs)),
            registry: deps.registry,
            db: deps.db,
            aha: deps.aha,
            tr064: deps.tr064,
            rate_limiter: deps.rate_limiter,
            weather_service: deps.weather_service,
            cycle_state: Mutex::new(WorkerCycleState {
                last_prune_date: init_prune,
                ..WorkerCycleState::default()
            }),
            logger: deps.logger,
        }
    }

    /// Updates `poll_interval_secs` and `rate_limit_window_secs` on `SIGHUP` reload.
    pub fn reconfigure(&self, poll_interval_secs: u64, rate_limit_window_secs: u64) {
        if let Ok(mut guard) = self.timing.write() {
            *guard = (poll_interval_secs.max(5), rate_limit_window_secs);
        }
    }

    /// Runs the periodic background polling loop until `shutdown_rx` signals `true`.
    pub async fn run_loop(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        let init_interval = self
            .timing
            .read()
            .map_or(DEFAULT_FRITZBOX_POLL_SECS, |g| g.0);
        self.logger.info(&format!(
            "Starting FritzBox polling loop (Interval: {init_interval}s)"
        ));
        if wait_or_stop(
            &mut shutdown_rx,
            Duration::from_secs(WORKER_STARTUP_DELAY_SECS),
        )
        .await
        {
            self.logger.info("Stopping FritzBox polling worker...");
            return;
        }
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let started = Instant::now();
            self.execute_poll_cycle().await;
            let (poll_secs, rl_win) = self.timing.read().map_or(
                (DEFAULT_FRITZBOX_POLL_SECS, DEFAULT_RATE_LIMIT_WINDOW_SECS),
                |g| *g,
            );
            self.cleanup_rate_limiter_if_due(started, rl_win);
            self.prune_history_if_due(now_epoch_f64()).await;

            let elapsed = started.elapsed().as_secs();
            let sleep_secs = poll_secs.saturating_sub(elapsed).max(WORKER_MIN_SLEEP_SECS);
            if wait_or_stop(&mut shutdown_rx, Duration::from_secs(sleep_secs)).await {
                break;
            }
        }
        self.logger.info("Stopping FritzBox polling worker...");
    }

    /// Executes one complete AHA + TR-064 + Presence + atomic SQLite commit poll cycle.
    pub async fn execute_poll_cycle(&self) {
        let now_ts = now_epoch_f64();
        let aha_res = self.aha.get_devices().await;
        let router_state = self.poll_router_device().await;
        let presence_pings = self.poll_configured_presence_macs().await;

        let parsed_list = match aha_res {
            Ok(list) => {
                if let Some(ref fw) = list.firmware_version {
                    self.tr064.set_fallback_firmware(Some(fw));
                }
                for shell_ain in &list.suppressed_shell_ains {
                    self.registry.remove_device(shell_ain);
                }
                Some(list.devices)
            }
            Err(err) => {
                self.logger
                    .error(&format!("Failed to poll Smart Home devices: {err}"));
                None
            }
        };

        if let Some(raw_devices) = parsed_list {
            self.process_and_commit_cycle(raw_devices, router_state, presence_pings, now_ts)
                .await;
        } else {
            self.registry
                .update_device(ROUTER_AIN, router_state.clone());
            self.commit_atomic_batch(Vec::new(), router_state, presence_pings, now_ts)
                .await;
        }
    }

    /// Syncs software groups, builds Digital Twin states, and commits one atomic SQLite tx.
    async fn process_and_commit_cycle(
        &self,
        raw_devices: Vec<RawAhaDevice>,
        router_state: DeviceState,
        presence_pings: Vec<(String, bool)>,
        now_ts: f64,
    ) {
        let groups_and_names = self
            .db
            .run_blocking(|db| {
                let groups = db.get_all_software_groups()?;
                let names = db.get_all_custom_names()?;
                Ok((groups, names))
            })
            .await;
        let (groups, custom_names) = match groups_and_names {
            Ok(pair) => pair,
            Err(err) => {
                self.logger.error(&format!(
                    "Failed loading SQLite groups/names during poll: {err}"
                ));
                return;
            }
        };

        self.sync_software_group_thermostats(&raw_devices, &groups, now_ts)
            .await;
        let sw_map = build_software_group_ain_map(&groups);

        let mut built: Vec<DeviceState> = {
            let db_ref = &self.db;
            raw_devices
                .iter()
                .map(|raw| {
                    let disp = resolve_display_name(
                        &raw.ain,
                        &raw.normalized_ain,
                        &raw.name,
                        &custom_names,
                    );
                    let sw_info = sw_map.get(&raw.normalized_ain);
                    let grp_ain = sw_info.map(|i| i.prefixed_id.clone());
                    let mut st = build_device_state(raw, disp, grp_ain, db_ref);
                    apply_software_group_authority(&mut st, sw_info);
                    self.warn_on_fritz_offset(&st);
                    st
                })
                .collect()
        };
        built.sort_by_key(|s| u8::from(s.device_type == device_types::THERMOSTAT));

        let baselines = self.load_missing_baselines(&built).await;
        self.apply_preservation_and_registry(&mut built, &baselines, now_ts);
        self.registry
            .update_device(ROUTER_AIN, router_state.clone());
        self.commit_atomic_batch(built, router_state, presence_pings, now_ts)
            .await;
        self.refresh_house_analytics().await;
    }

    /// Applies target/sensor preservation onto `built` and updates `StateRegistry`.
    fn apply_preservation_and_registry(
        &self,
        built: &mut [DeviceState],
        baselines: &HashMap<String, Map<String, Value>>,
        now_ts: f64,
    ) {
        let mut last_targets = self
            .cycle_state
            .lock()
            .map(|g| g.last_target_temps.clone())
            .unwrap_or_default();
        for st in built {
            let bmap = baselines.get(&st.ain);
            preserve_enriched_twin_state(
                st,
                &self.registry,
                bmap,
                self.weather_service.as_deref(),
                &mut last_targets,
                now_ts,
            );
            self.registry.update_device(&st.ain, st.clone());
        }
        if let Ok(mut guard) = self.cycle_state.lock() {
            guard.last_target_temps = last_targets;
        }
    }

    /// Mirrors physical radiator dial changes across members of the same software group.
    async fn sync_software_group_thermostats(
        &self,
        devices: &[RawAhaDevice],
        groups: &[Value],
        now_ts: f64,
    ) {
        if groups.is_empty() {
            return;
        }
        let mut current_targets = HashMap::new();
        let mut last_targets = self
            .cycle_state
            .lock()
            .map(|g| g.last_target_temps.clone())
            .unwrap_or_default();

        for dev in devices {
            if (dev.bitmask & fritz_bitmask::THERMOSTAT) == 0 && dev.hkr_temp.is_none() {
                continue;
            }
            let raw_t = dev.target_temperature.unwrap_or(0.0);
            let eff_t =
                resolve_effective_user_target(&self.registry, &dev.ain, raw_t, None, now_ts);
            current_targets.insert(dev.normalized_ain.clone(), eff_t);
            if let Some(prev) = self.registry.get_device(&dev.ain) {
                if prev
                    .as_thermostat()
                    .is_some_and(|t| t.target_hold_until > now_ts)
                {
                    last_targets.insert(dev.normalized_ain.clone(), eff_t);
                }
            }
        }

        for grp in groups {
            let gname = grp.get("name").and_then(Value::as_str).unwrap_or("Unknown");
            let Some(members) = grp.get("members").and_then(Value::as_array) else {
                continue;
            };
            let norm_members: Vec<String> = members
                .iter()
                .filter_map(Value::as_str)
                .map(normalize_ain)
                .collect();
            let changed_src = norm_members.iter().find_map(|norm| {
                let cur = *current_targets.get(norm)?;
                let prev = *last_targets.get(norm)?;
                ((cur - prev).abs() > TEMP_CHANGE_THRESHOLD).then(|| (norm.clone(), cur))
            });
            if let Some((src_ain, target_temp)) = changed_src {
                self.mirror_group_target(
                    gname,
                    &norm_members,
                    &src_ain,
                    target_temp,
                    &mut current_targets,
                    &mut last_targets,
                    now_ts,
                )
                .await;
            }
        }
        last_targets.extend(current_targets);
        if let Ok(mut guard) = self.cycle_state.lock() {
            guard.last_target_temps = last_targets;
        }
    }

    /// Sends `set_target_temperature` to all sibling thermostats in `norm_members`.
    #[allow(clippy::too_many_arguments)]
    async fn mirror_group_target(
        &self,
        group_name: &str,
        norm_members: &[String],
        source_ain: &str,
        target_temp: f64,
        current_targets: &mut HashMap<String, f64>,
        last_targets: &mut HashMap<String, f64>,
        now_ts: f64,
    ) {
        self.logger.info(&format!(
            "Auto-Sync: Group '{group_name}' changed on {source_ain} -> {target_temp}°C"
        ));
        for sib in norm_members {
            if sib == source_ain {
                continue;
            }
            let Some(&sib_cur) = current_targets.get(sib) else {
                continue;
            };
            if (sib_cur - target_temp).abs() <= TEMP_CHANGE_THRESHOLD {
                continue;
            }
            match self.aha.set_target_temperature(sib, target_temp).await {
                Ok(()) => {
                    current_targets.insert(sib.clone(), target_temp);
                    last_targets.insert(sib.clone(), target_temp);
                    self.registry.mutate_device(sib, |dev| {
                        if let Some(t) = dev.as_thermostat_mut() {
                            t.temperature_target = target_temp;
                            t.hardware_target_temperature = Some(target_temp);
                            t.target_hold_until = now_ts + TARGET_HOLD_SECONDS;
                        }
                    });
                }
                Err(err) => {
                    self.logger.warning(&format!(
                        "Auto-Sync failed for {sib} in group '{group_name}': {err}"
                    ));
                }
            }
        }
    }

    /// Loads SQLite baseline snapshots only for thermostats not yet present in `StateRegistry`.
    async fn load_missing_baselines(
        &self,
        built: &[DeviceState],
    ) -> HashMap<String, Map<String, Value>> {
        let missing: Vec<DeviceState> = built
            .iter()
            .filter(|s| {
                s.device_type == device_types::THERMOSTAT
                    && self.registry.get_device(&s.ain).is_none()
            })
            .cloned()
            .collect();
        if missing.is_empty() {
            return HashMap::new();
        }
        self.db
            .run_blocking(move |db| {
                let mut out = HashMap::new();
                for dev in missing {
                    if let Ok(Some(map)) = db.load_initial_device_baseline(&dev) {
                        out.insert(dev.ain, map);
                    }
                }
                Ok(out)
            })
            .await
            .unwrap_or_default()
    }

    /// Commits all device snapshots, `ROUTER_01`, and Wi-Fi presence pings in a single SQLite tx.
    async fn commit_atomic_batch(
        &self,
        devices: Vec<DeviceState>,
        router_state: DeviceState,
        presence_pings: Vec<(String, bool)>,
        now_ts: f64,
    ) {
        let res = self
            .db
            .run_blocking(move |db| {
                db.with_tx(|tx| {
                    for dev in &devices {
                        db.persist_device_history_on(tx, dev, Some(now_ts))?;
                    }
                    db.persist_device_history_on(tx, &router_state, Some(now_ts))?;
                    for (mac, is_home) in &presence_pings {
                        db.insert_presence_ping_on(tx, mac, *is_home, Some(now_ts))?;
                    }
                    Ok(())
                })
            })
            .await;
        if let Err(err) = res {
            self.logger
                .error(&format!("Atomic poll history transaction failed: {err}"));
        }
    }
}

// =============================================================================
/// Sleeps for `duration` or returns `true` immediately when `shutdown_rx` signals shutdown.
async fn wait_or_stop(shutdown_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => *shutdown_rx.borrow(),
        res = shutdown_rx.changed() => res.is_err() || *shutdown_rx.borrow(),
    }
}
