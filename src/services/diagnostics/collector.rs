//! Universal Diagnostics Schema v1.0 5-block domain collector (`DiagnosticsService`).
//!
//! Assembles `meta`, `config_sanitized`, `health_checks`, `active_alerts`,
//! `domain_diagnostics` (Digital Twin, `decision_trace`, `time_series_analysis`),
//! `recent_errors`, and `recent_logs` for REST API and CLI consumers.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Map, Value};

use super::alerts::{
    build_full_health_checks, collect_active_alerts, resolve_overall_health_status,
};
use super::series::{
    analyze_single_entity, load_offline_devices_from_db, resolve_target_identifiers,
    serialize_live_device,
};
use crate::core::config::AppConfig;
use crate::core::constants::{
    app_meta::{SOFTWARE_NAME, SYSLOG_IDENTIFIER, VERSION},
    diagnostics_defaults::{DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT, SCHEMA_VERSION},
};
use crate::core::diagnostics::{
    collect_recent_logs, process_uptime_seconds, redact_sensitive_json, validate_diagnostics_range,
    DiagnosticsQuery,
};
use crate::core::domain_constants::{
    automation::{
        automation_identifiers::SCOPE_GLOBAL,
        weather_defaults::{TIMEZONE, TIMEZONE_AUTO},
    },
    hardware::special_devices::WEATHER_AIN,
};
use crate::core::error::{AppError, AppResult};
use crate::core::heating_forecast::ForecastDayInput;
use crate::core::logger::Logger;
use crate::core::psychrometrics::{calculate_absolute_humidity, calculate_dew_point};
use crate::core::time_util::{current_os_timezone_abbr, now_utc_iso8601, resolve_home_tz};
use crate::db::efficiency::AnalyticsContext;
use crate::db::Database;
use crate::models::registry::StateRegistry;
use crate::services::automation::RulesEngine;
use crate::services::rate_limiter::RateLimiter;
use crate::services::weather::WeatherService;

// =============================================================================
/// Domain-aware Universal Diagnostics Schema v1.0 collector for REST API & CLI.
#[derive(Debug, Clone)]
pub struct DiagnosticsService {
    /// Active application configuration.
    config: AppConfig,
    /// Optional SQLite database handle.
    db: Option<Database>,
    /// Optional live in-memory Digital Twin registry.
    registry: Option<Arc<StateRegistry>>,
    /// Optional live `RulesEngine` instance.
    rules_engine: Option<Arc<RulesEngine>>,
    /// Optional live Open-Meteo `WeatherService`.
    weather_service: Option<Arc<WeatherService>>,
    /// Optional live sliding-window `RateLimiter`.
    rate_limiter: Option<Arc<RateLimiter>>,
    /// Application logger.
    logger: Logger,
}

impl DiagnosticsService {
    /// Creates a new [`DiagnosticsService`] wired to available runtime services.
    #[must_use]
    pub fn new(
        config: AppConfig,
        db: Option<Database>,
        registry: Option<Arc<StateRegistry>>,
        rules_engine: Option<Arc<RulesEngine>>,
        weather_service: Option<Arc<WeatherService>>,
        rate_limiter: Option<Arc<RateLimiter>>,
        logger: Logger,
    ) -> Self {
        Self {
            config,
            db,
            registry,
            rules_engine,
            weather_service,
            rate_limiter,
            logger,
        }
    }

    /// Collects the complete sanitized 5-block diagnostics report synchronously (CLI-safe).
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `query.range` is not a supported range token.
    pub fn collect_report(&self, query: &DiagnosticsQuery) -> AppResult<Value> {
        let eff_range = validate_diagnostics_range(query.range.as_deref())?;
        let log_limit = query
            .limit
            .unwrap_or(DEFAULT_LOG_LIMIT)
            .clamp(1, MAX_LOG_LIMIT);
        let live_trace = self
            .rules_engine
            .as_ref()
            .map_or_else(|| json!({}), |e| e.get_last_evaluation_trace());

        let health = build_full_health_checks(
            &self.config,
            self.db.as_ref(),
            self.weather_service.as_deref(),
            self.rate_limiter.as_deref(),
        );
        let domain =
            self.collect_domain_diagnostics(query.device.as_deref(), eff_range, live_trace);
        let active_alerts = collect_active_alerts(&health, &domain, self.weather_service.is_some());
        let status = resolve_overall_health_status(&health, &active_alerts);
        let cfg_json = serde_json::to_value(&self.config).unwrap_or_else(|_| json!({}));

        let raw = json!({
            "schema_version": SCHEMA_VERSION,
            "status": status,
            "meta": self.build_meta(eff_range, query.device.as_deref()),
            "config_sanitized": redact_sensitive_json(&cfg_json),
            "health_checks": health,
            "active_alerts": active_alerts,
            "domain_diagnostics": domain,
            "recent_errors": collect_recent_logs(&self.logger, log_limit, true),
            "recent_logs": collect_recent_logs(&self.logger, log_limit, false),
        });
        Ok(redact_sensitive_json(&raw))
    }

    /// Collects the 5-block diagnostics report on `tokio`'s blocking pool for `axum` handlers.
    ///
    /// # Errors
    /// Returns [`AppError`] on invalid `query.range` or blocking task failure.
    pub async fn collect_report_async(&self, query: DiagnosticsQuery) -> AppResult<Value> {
        validate_diagnostics_range(query.range.as_deref())?;
        let svc = self.clone();
        tokio::task::spawn_blocking(move || svc.collect_report(&query))
            .await
            .map_err(|err| AppError::Service(format!("Diagnostics worker failed: {err}")))?
    }

    /// Builds Block 1 (`meta`) including home IANA timezone and `live_memory_attached`.
    fn build_meta(&self, range_key: &str, device_filter: Option<&str>) -> Value {
        let tz_name = self.resolve_home_tz_name();
        let home_tz = resolve_home_tz(Some(&tz_name));
        let now_local = Utc::now().with_timezone(&home_tz).to_rfc3339();

        json!({
            "software": SOFTWARE_NAME,
            "version": VERSION,
            "syslog_identifier": SYSLOG_IDENTIFIER,
            "generated_at_utc": now_utc_iso8601(),
            "generated_at_local": now_local,
            "timezone_iana": tz_name,
            "os_timezone": current_os_timezone_abbr(),
            "uptime_seconds": process_uptime_seconds(),
            "daemon_mode": self.config.daemon.enabled,
            "live_memory_attached": self.registry.is_some(),
            "arch": std::env::consts::ARCH,
            "requested_range": range_key,
            "device_filter": device_filter,
        })
    }

    /// Assembles Block 4 (`domain_diagnostics`: `digital_twin`, `decision_trace`, `time_series_analysis`).
    fn collect_domain_diagnostics(
        &self,
        device_filter: Option<&str>,
        range_str: &str,
        live_trace: Value,
    ) -> Value {
        let tz_name = self.resolve_home_tz_name();
        let (daily_fc, live_out) = self.extract_forecast_and_outdoor();
        let twin = self.collect_digital_twin_snapshot(&daily_fc, live_out, &tz_name);
        let traces = self.collect_decision_traces(live_trace);
        let targets = resolve_target_identifiers(&twin, device_filter);

        let mut series_map = Map::with_capacity(targets.len());
        for id in targets {
            let entry = analyze_single_entity(
                &id,
                range_str,
                self.db.as_ref(),
                self.registry.as_deref(),
                &daily_fc,
                &tz_name,
                live_out,
            );
            series_map.insert(id, entry);
        }

        json!({
            "digital_twin": twin,
            "decision_trace": traces,
            "time_series_analysis": series_map,
        })
    }

    /// Collects Digital Twin state from live `StateRegistry` or offline SQLite fallback.
    fn collect_digital_twin_snapshot(
        &self,
        daily_fc: &[ForecastDayInput],
        live_out: Option<f64>,
        tz_name: &str,
    ) -> Value {
        let groups = self
            .db
            .as_ref()
            .and_then(|d| d.get_all_software_groups().ok())
            .unwrap_or_default();

        if let Some(ref reg) = self.registry {
            let devices: Vec<Value> = reg
                .get_all_devices()
                .iter()
                .map(serialize_live_device)
                .collect();
            let live_targets = reg.get_live_thermostat_targets();
            let eff = reg.get_heating_efficiency().or_else(|| {
                let d = self.db.as_ref()?;
                let ctx = AnalyticsContext {
                    since: None,
                    range_str: None,
                    daily_forecast: daily_fc,
                    live_targets: Some(&live_targets),
                    live_outdoor_temp: live_out,
                };
                d.get_house_heating_efficiency(&ctx)
            });
            let live_sw = reg.get_live_switch_states();
            let energy = reg.get_energy_summary().or_else(|| {
                self.db
                    .as_ref()
                    .and_then(|d| d.get_house_energy_summary(Some(tz_name), None, Some(&live_sw)))
            });
            let weather = self
                .weather_service
                .as_deref()
                .and_then(WeatherService::get_current_weather);
            return json!({
                "source": "live_registry",
                "devices": devices,
                "software_groups": groups,
                "weather": weather,
                "house_heating_efficiency": eff,
                "house_energy_summary": energy,
            });
        }

        let devices = self
            .db
            .as_ref()
            .map_or_else(Vec::new, load_offline_devices_from_db);
        let eff = self.db.as_ref().and_then(|d| {
            let ctx = AnalyticsContext {
                since: None,
                range_str: None,
                daily_forecast: daily_fc,
                live_targets: None,
                live_outdoor_temp: live_out,
            };
            d.get_house_heating_efficiency(&ctx)
        });
        let energy = self
            .db
            .as_ref()
            .and_then(|d| d.get_house_energy_summary(Some(tz_name), None, None));
        let weather = self
            .db
            .as_ref()
            .and_then(|d| d.get_latest_state_snapshot(WEATHER_AIN).ok().flatten())
            .map(|(snap, _)| enrich_offline_weather(Value::Object(snap)));

        json!({
            "source": "sqlite_fallback",
            "devices": devices,
            "software_groups": groups,
            "weather": weather,
            "house_heating_efficiency": eff,
            "house_energy_summary": energy,
        })
    }

    /// Collects live `RulesEngine` trace plus persisted `global` & `device` rule configurations.
    fn collect_decision_traces(&self, live_trace: Value) -> Value {
        let global_rules = self
            .db
            .as_ref()
            .and_then(|d| {
                d.get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)
                    .ok()
            })
            .unwrap_or_else(|| json!({}));
        let device_rules: HashMap<String, Value> = self
            .db
            .as_ref()
            .and_then(|d| d.get_all_device_scope_configs().ok())
            .unwrap_or_default();

        json!({
            "live_engine_trace": live_trace,
            "configured_global_rules": global_rules,
            "configured_device_rules": device_rules,
        })
    }

    /// Resolves configured or Open-Meteo-inferred home IANA timezone name.
    fn resolve_home_tz_name(&self) -> String {
        if let Some(ref ws) = self.weather_service {
            let tz = ws.get_timezone_name();
            if !tz.is_empty() {
                return tz;
            }
        }
        let cfg_tz = self.config.weather.timezone.trim();
        if !cfg_tz.is_empty() && !cfg_tz.eq_ignore_ascii_case(TIMEZONE_AUTO) {
            return cfg_tz.to_owned();
        }
        TIMEZONE.to_owned()
    }

    /// Extracts `(daily_forecast, live_outdoor_temp)` from `WeatherService`.
    fn extract_forecast_and_outdoor(&self) -> (Vec<ForecastDayInput>, Option<f64>) {
        let Some(ref ws) = self.weather_service else {
            return (Vec::new(), None);
        };
        let snap = ws.get_snapshot();
        let out_t = snap.current.as_ref().and_then(|c| c.temperature);
        (snap.to_forecast_inputs(), out_t)
    }
}

// =============================================================================
/// Attaches `dew_point_c` and `abs_humidity_gm3` onto an offline `WEATHER_01` SQLite snapshot.
fn enrich_offline_weather(mut snap: Value) -> Value {
    if let Some(obj) = snap.as_object_mut() {
        let t_opt = obj.get("temperature").and_then(Value::as_f64);
        let rh_opt = obj.get("humidity").and_then(Value::as_f64);
        if let (Some(t), Some(rh)) = (t_opt, rh_opt.filter(|&r| r > 0.0)) {
            obj.entry(String::from("dew_point_c"))
                .or_insert_with(|| json!(calculate_dew_point(t, rh)));
            obj.entry(String::from("abs_humidity_gm3"))
                .or_insert_with(|| json!(calculate_absolute_humidity(t, rh)));
        }
    }
    snap
}
