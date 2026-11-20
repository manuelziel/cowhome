//! Async background `RulesEngine` orchestrating CowHome's 7 smart automation rules.
//!
//! Evaluates `Global` -> `Group` -> `Standalone Device` -> `Global Fallback` scopes
//! under the Single-Authority Pattern (`"Group overrides Room"`), delegating the 5-tier
//! per-scope rule chain to [`super::chain`] and [`super::chain_climate`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use tokio::sync::{watch, Mutex};

use super::chain::extract_group_members;
use super::scopes::{
    clear_reference_sensors, parse_feature_cfg, restore_unlinked_thermostats, ScopeConfigMap,
};
use super::trace::{
    build_cycle_evaluation_trace, resolve_device_schedule_slot_id,
    sync_thermostat_group_paused_reasons, RuleSet,
};
use crate::core::constants::service_defaults::{
    WORKER_MIN_SLEEP_SECS as MIN_SLEEP_SECS, WORKER_STARTUP_DELAY_SECS as STARTUP_DELAY_SECS,
};
use crate::core::domain_constants::{
    automation::{
        automation_defaults::ENGINE_INTERVAL_SECS,
        automation_identifiers::{
            APPARENT_TEMPERATURE, SCOPE_DEVICE, SCOPE_GLOBAL, SCOPE_GROUP, VACATION_MODE,
        },
    },
    hardware::device_types,
};
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, resolve_home_tz};
use crate::db::efficiency::AnalyticsContext;
use crate::db::Database;
use crate::models::automation::{ApparentTemperatureConfig, VacationModeConfig};
use crate::models::registry::StateRegistry;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client};
use crate::services::weather::WeatherService;

/// Startup wait attempts (`10` × `2 s`) for `FritzPollingWorker` to populate `StateRegistry`.
const STARTUP_WAIT_ATTEMPTS: u32 = 10;

// =============================================================================
/// Async `RulesEngine` shared across `axum` handlers, CLI, and daemon tasks.
#[derive(Debug)]
pub struct RulesEngine {
    /// Serialized rule set guarded by an async mutex so `evaluate_now` & overrides never race.
    pub(super) rules: Mutex<RuleSet>,
    /// Evaluation interval in seconds (hot-reloadable on `SIGHUP`).
    interval_secs: AtomicU64,
    /// SQLite database handle.
    pub(super) db: Database,
    /// Digital Twin state registry.
    pub(super) registry: Arc<StateRegistry>,
    /// FritzBox AHA HTTP client.
    pub(super) aha: Arc<FritzAhaClient>,
    /// FritzBox TR-064 SOAP client.
    pub(super) tr064: Arc<FritzTr064Client>,
    /// Optional Open-Meteo weather client.
    pub(super) weather_service: Option<Arc<WeatherService>>,
    /// Most recent structured evaluation trace for `/system/diagnostics` & `/system/ai-diagnostics`.
    last_trace: RwLock<Value>,
    /// Application logger.
    pub(super) logger: Logger,
}

impl RulesEngine {
    /// Creates a new [`RulesEngine`] instance.
    #[must_use]
    pub fn new(
        interval_secs: u64,
        db: Database,
        registry: Arc<StateRegistry>,
        aha: Arc<FritzAhaClient>,
        tr064: Arc<FritzTr064Client>,
        weather_service: Option<Arc<WeatherService>>,
        logger: Logger,
    ) -> Self {
        let rules = RuleSet::new(&logger);
        Self {
            rules: Mutex::new(rules),
            interval_secs: AtomicU64::new(interval_secs.max(MIN_SLEEP_SECS)),
            db,
            registry,
            aha,
            tr064,
            weather_service,
            last_trace: RwLock::new(json!({})),
            logger,
        }
    }

    /// Hot-reloads the `RulesEngine` evaluation interval on `SIGHUP`.
    pub fn reconfigure(&self, interval_secs: u64) {
        self.interval_secs
            .store(interval_secs.max(MIN_SLEEP_SECS), Ordering::Relaxed);
    }

    /// Returns a cloned JSON snapshot of the most recent evaluation cycle trace.
    #[must_use]
    pub fn get_last_evaluation_trace(&self) -> Value {
        self.last_trace
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| json!({}))
    }

    /// Triggers an immediate serialized rule evaluation cycle across all scopes.
    pub async fn evaluate_now(&self) {
        let mut guard = self.rules.lock().await;
        self.evaluate_all_scopes(&mut guard).await;
    }

    /// Clears all cached `HeatingSchedule` slots and `AdaptiveStart` latches.
    pub async fn invalidate_schedule_cache(&self) {
        let mut guard = self.rules.lock().await;
        guard.schedule.invalidate_all();
        let all_ains: Vec<String> = self
            .registry
            .get_all_devices()
            .into_iter()
            .map(|d| d.ain)
            .collect();
        guard
            .adaptive_start
            .clear_devices(&all_ains, false, Some(&self.registry));
    }

    /// Invalidates cached schedule & pre-heat state for a specific `(scope_type, scope_id)`.
    pub async fn invalidate_schedule_scope(&self, scope_type: &str, scope_id: &str) {
        let mut guard = self.rules.lock().await;
        if scope_type == SCOPE_GROUP && !scope_id.is_empty() {
            let sid = scope_id.to_owned();
            let grp = self
                .db
                .run_blocking(move |d| d.get_software_group(&sid))
                .await
                .ok()
                .flatten();
            let members = extract_group_members(grp.as_ref());
            guard.schedule.invalidate_devices(&members);
            guard
                .adaptive_start
                .clear_devices(&members, false, Some(&self.registry));
        } else if scope_type == SCOPE_DEVICE && !scope_id.is_empty() {
            let slice = [scope_id.to_owned()];
            guard.schedule.invalidate_devices(&slice);
            guard
                .adaptive_start
                .clear_devices(&slice, false, Some(&self.registry));
        } else {
            guard.schedule.invalidate_all();
            let all_ains: Vec<String> = self
                .registry
                .get_all_devices()
                .into_iter()
                .map(|d| d.ain)
                .collect();
            guard
                .adaptive_start
                .clear_devices(&all_ains, false, Some(&self.registry));
        }
    }

    /// Latches a manual user override on `ains` in their active `HeatingSchedule` phase.
    pub async fn notify_manual_override(&self, ains: &[String]) {
        let mut guard = self.rules.lock().await;
        guard
            .adaptive_start
            .record_manual_override(ains, Some(&self.registry));
        let now_dt = self.now_local();
        for ain in ains {
            let slot_id =
                resolve_device_schedule_slot_id(ain, &self.db, &self.registry, now_dt).await;
            guard
                .schedule
                .record_manual_override(std::slice::from_ref(ain), slot_id.as_deref());
        }
    }

    /// Runs the background periodic evaluation loop until `shutdown_rx` signals `true`.
    pub async fn run_loop(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        let iv = self.interval_secs.load(Ordering::Relaxed);
        self.logger
            .info(&format!("Starting RulesEngine (Interval: {iv}s)..."));
        let mut attempts = 0_u32;
        while !*shutdown_rx.borrow()
            && self.registry.get_all_devices().is_empty()
            && attempts < STARTUP_WAIT_ATTEMPTS
        {
            if wait_or_stop(&mut shutdown_rx, Duration::from_secs(STARTUP_DELAY_SECS)).await {
                self.logger.info("RulesEngine stopped.");
                return;
            }
            attempts += 1;
        }

        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let started = Instant::now();
            self.logger.debug("Starting rule evaluation cycle...");
            self.evaluate_now().await;
            let interval = self
                .interval_secs
                .load(Ordering::Relaxed)
                .max(ENGINE_INTERVAL_SECS.min(MIN_SLEEP_SECS));
            let sleep_secs = interval
                .saturating_sub(started.elapsed().as_secs())
                .max(MIN_SLEEP_SECS);
            if wait_or_stop(&mut shutdown_rx, Duration::from_secs(sleep_secs)).await {
                break;
            }
        }
        self.logger.info("RulesEngine stopped.");
    }

    /// Resolves current local wall-clock [`DateTime<Tz>`] in the home IANA timezone.
    #[must_use]
    pub fn now_local(&self) -> DateTime<Tz> {
        let tz = match &self.weather_service {
            Some(ws) => ws.get_tz(),
            None => {
                let fallback = self.db.default_timezone();
                resolve_home_tz(Some(&fallback))
            }
        };
        Utc::now().with_timezone(&tz)
    }

    /// Evaluates Global Guards, Group Scopes, Standalone Devices, and Global Fallbacks.
    async fn evaluate_all_scopes(&self, rules: &mut RuleSet) {
        let (groups, device_map, global_configs) = self.load_db_scopes().await;
        rules.weather.last_suppressed_ains.clear();
        rules.presence.begin_cycle();
        let mut scope_traces = Vec::new();

        rules
            .humidity
            .check_expired_boosts(&self.registry, &self.aha, None)
            .await;
        let vacation_locked = self.evaluate_global_guards(rules, &global_configs).await;
        let mut managed = self
            .evaluate_group_scopes(
                rules,
                &groups,
                &global_configs,
                &vacation_locked,
                &mut scope_traces,
            )
            .await;
        self.evaluate_device_scopes(
            rules,
            &device_map,
            &global_configs,
            &vacation_locked,
            &mut managed,
            &mut scope_traces,
        )
        .await;
        let unmanaged = self.collect_unmanaged_thermostats(&managed);
        self.evaluate_scope_chain(
            rules,
            global_configs,
            &unmanaged,
            &vacation_locked,
            true,
            SCOPE_GLOBAL,
            SCOPE_GLOBAL,
            &mut scope_traces,
        )
        .await;
        self.finalize_evaluation_cycle(rules, &groups, &vacation_locked, scope_traces)
            .await;
    }

    /// Collects ungrouped thermostats without an explicit `device` scope for global fallback.
    fn collect_unmanaged_thermostats(&self, managed: &HashSet<String>) -> Vec<String> {
        self.registry
            .get_all_devices()
            .into_iter()
            .filter(|d| {
                !managed.contains(&normalize_ain(&d.ain))
                    && matches!(
                        d.device_type.as_str(),
                        device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
                    )
            })
            .map(|d| d.ain)
            .collect()
    }

    /// Syncs group paused reasons, unlinked calibrations, house efficiency, & cycle trace.
    async fn finalize_evaluation_cycle(
        &self,
        rules: &mut RuleSet,
        groups: &[Value],
        vacation_locked: &HashSet<String>,
        scope_traces: Vec<Value>,
    ) {
        sync_thermostat_group_paused_reasons(groups, &self.registry);
        restore_unlinked_thermostats(
            &rules.apparent_temp,
            &self.registry,
            self.weather_service.as_deref(),
            &self.aha,
        )
        .await;
        self.update_house_heating_efficiency().await;

        let trace = build_cycle_evaluation_trace(
            rules,
            vacation_locked,
            scope_traces,
            &self.registry,
            self.now_local(),
        );
        if let Ok(mut g) = self.last_trace.write() {
            *g = trace;
        }
        rules.presence.end_cycle();
    }

    /// Evaluates house-wide `VacationMode` guard and global `ApparentTemperature` enrichment.
    async fn evaluate_global_guards(
        &self,
        rules: &mut RuleSet,
        global_configs: &ScopeConfigMap,
    ) -> HashSet<String> {
        clear_reference_sensors(&self.registry, &self.db).await;
        let (app_on, app_cfg) =
            parse_feature_cfg::<ApparentTemperatureConfig>(global_configs, APPARENT_TEMPERATURE);
        if app_on {
            rules.apparent_temp.enrich_only(&app_cfg, &self.registry);
        }
        let (vac_on, vac_cfg) =
            parse_feature_cfg::<VacationModeConfig>(global_configs, VACATION_MODE);
        if !vac_on {
            return HashSet::new();
        }
        rules
            .vacation
            .evaluate(&vac_cfg, &self.registry, &self.aha, &self.db)
            .await;
        rules.vacation.locked_ains.clone()
    }

    /// Loads `(groups, device_scope_map, global_configs)` from SQLite in one blocking hop.
    async fn load_db_scopes(&self) -> (Vec<Value>, HashMap<String, Value>, ScopeConfigMap) {
        self.db
            .run_blocking(|d| {
                let groups = d.get_all_software_groups().unwrap_or_default();
                let devs = d.get_all_device_scope_configs().unwrap_or_default();
                let glob = d
                    .get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)
                    .ok()
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                Ok((groups, devs, glob))
            })
            .await
            .unwrap_or_default()
    }

    /// Refreshes cached house-wide `heating_efficiency` (+ 5-day forecast) in `StateRegistry`.
    async fn update_house_heating_efficiency(&self) {
        let (daily_fc, live_out) = match &self.weather_service {
            Some(ws) => {
                let snap = ws.get_snapshot();
                let out_t = snap.current.as_ref().and_then(|c| c.temperature);
                (snap.to_forecast_inputs(), out_t)
            }
            None => (Vec::new(), None),
        };
        let live_targets = self.registry.get_live_thermostat_targets();
        if let Ok(eff) = self
            .db
            .run_blocking(move |d| {
                let ctx = AnalyticsContext {
                    since: None,
                    range_str: None,
                    daily_forecast: &daily_fc,
                    live_targets: Some(&live_targets),
                    live_outdoor_temp: live_out,
                };
                Ok(d.get_house_heating_efficiency(&ctx))
            })
            .await
        {
            self.registry.set_heating_efficiency(eff);
        }
    }
}

// =============================================================================
/// Sleeps for `duration` or returns `true` immediately when `shutdown_rx` turns `true`.
async fn wait_or_stop(shutdown_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => *shutdown_rx.borrow(),
        changed = shutdown_rx.changed() => changed.is_err() || *shutdown_rx.borrow(),
    }
}
