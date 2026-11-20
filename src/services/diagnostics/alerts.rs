//! Subsystem health checks & `ANSI/ISA-18.2` hardware/system alert derivation.
//!
//! Builds Block 3 (`health_checks`) and `active_alerts` (`device_offline`,
//! `battery_low`, `valve_error_1..6`, `fritz_offset_active`, `weather_service_degraded`,
//! `database_unhealthy`, `storage_space_low`, `systemd_service_degraded`).

use std::time::Duration;

use serde_json::{json, Value};

use crate::core::config::AppConfig;
use crate::core::constants::{
    app_meta::SYSLOG_IDENTIFIER,
    diagnostics_defaults::{
        ALERT_BATTERY_LOW, ALERT_DB_UNHEALTHY, ALERT_DEVICE_OFFLINE, ALERT_FRITZ_OFFSET_ACTIVE,
        ALERT_SERVICE_DEGRADED, ALERT_STORAGE_LOW, ALERT_VALVE_ERROR_PREFIX,
        ALERT_WEATHER_DEGRADED, DISK_FAIL_PERCENT, DISK_WARN_PERCENT,
    },
};
use crate::core::domain_constants::hardware::{
    device_types,
    fritz_api::thermostat_error_description,
    special_devices::{WEATHER_AIN, WEATHER_NAME},
};
use crate::core::process::run_command_captured;
use crate::core::psychrometrics::round_decimals;
use crate::db::Database;
use crate::services::automation::actuation::RULE_TEMP_EPSILON;
use crate::services::rate_limiter::RateLimiter;
use crate::services::weather::WeatherService;

/// Bytes per mebibyte (`1_048_576`).
const BYTES_PER_MIB: f64 = 1_048_576.0;
/// Bytes per gibibyte (`1_073_741_824`).
const BYTES_PER_GIB: f64 = 1_073_741_824.0;

// =============================================================================
/// Builds Block 3 (`health_checks`) across `database`, `storage`, `systemd_service`, & `adapters`.
#[must_use]
pub fn build_full_health_checks(
    config: &AppConfig,
    db: Option<&Database>,
    weather: Option<&WeatherService>,
    rate_limiter: Option<&RateLimiter>,
) -> Value {
    let (db_chk, storage_chk) = collect_db_and_storage_checks(db);
    json!({
        "database": db_chk,
        "storage": storage_chk,
        "systemd_service": query_systemd_service_state(SYSLOG_IDENTIFIER),
        "adapters": collect_adapter_checks(config, weather, rate_limiter),
    })
}

// =============================================================================
/// Derives noise-free `ANSI/ISA-18.2` system, adapter, and Smart Home device alerts.
#[must_use]
pub fn collect_active_alerts(health: &Value, domain: &Value, has_weather: bool) -> Vec<Value> {
    let mut alerts = Vec::new();
    for (chk_key, code, label) in [
        ("database", ALERT_DB_UNHEALTHY, "SQLite database"),
        ("storage", ALERT_STORAGE_LOW, "Storage"),
        ("systemd_service", ALERT_SERVICE_DEGRADED, "Systemd service"),
    ] {
        let Some(chk) = health.get(chk_key) else {
            continue;
        };
        let st = chk.get("status").and_then(Value::as_str).unwrap_or("pass");
        if matches!(st, "warn" | "fail") {
            let sev = if st == "fail" { "error" } else { "warning" };
            let msg = chk
                .get("error")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{label} status is '{st}'"), str::to_owned);
            alerts.push(json!({
                "severity": sev,
                "category": "system",
                "code": code,
                "device": Value::Null,
                "name": chk_key,
                "message": msg,
            }));
        }
    }

    if has_weather {
        if let Some(ws_chk) = health.pointer("/adapters/weather_service") {
            let st = ws_chk
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pass");
            if matches!(st, "warn" | "fail") {
                let pstate = ws_chk
                    .get("provider_state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                alerts.push(json!({
                    "severity": "warning",
                    "category": "adapter",
                    "code": ALERT_WEATHER_DEGRADED,
                    "device": WEATHER_AIN,
                    "name": WEATHER_NAME,
                    "message": format!("Weather service state is '{pstate}'"),
                }));
            }
        }
    }

    if let Some(devs) = domain
        .pointer("/digital_twin/devices")
        .and_then(Value::as_array)
    {
        alerts.extend(build_device_alerts(devs));
    }
    alerts
}

// =============================================================================
/// Computes top-level `"pass" | "warn" | "fail"` health status from checks & alerts.
#[must_use]
pub fn resolve_overall_health_status(health: &Value, alerts: &[Value]) -> &'static str {
    let mut statuses: Vec<&str> = [
        "/database/status",
        "/storage/status",
        "/systemd_service/status",
    ]
    .into_iter()
    .filter_map(|p| health.pointer(p).and_then(Value::as_str))
    .collect();
    if let Some(adapters) = health.get("adapters").and_then(Value::as_object) {
        for val in adapters.values() {
            if let Some(s) = val.get("status").and_then(Value::as_str) {
                statuses.push(s);
            }
        }
    }
    let has_err = alerts
        .iter()
        .any(|a| a.get("severity").and_then(Value::as_str) == Some("error"));
    if statuses.contains(&"fail") || has_err {
        return "fail";
    }
    let has_warn = alerts
        .iter()
        .any(|a| a.get("severity").and_then(Value::as_str) == Some("warning"));
    if statuses.contains(&"warn") || has_warn {
        return "warn";
    }
    "pass"
}

// =============================================================================
/// Collects `database` and `storage` health blocks via `Database::get_storage_stats`.
fn collect_db_and_storage_checks(db: Option<&Database>) -> (Value, Value) {
    let Some(database) = db else {
        return (
            json!({ "status": "fail", "attached": false, "error": "Database not initialized" }),
            json!({ "status": "warn", "error": "Storage stats unavailable" }),
        );
    };
    let stats = match database.get_storage_stats() {
        Ok(s) => s,
        Err(err) => {
            let msg = err.to_string();
            return (
                json!({ "status": "fail", "attached": true, "error": msg }),
                json!({ "status": "warn", "error": msg }),
            );
        }
    };
    let db_info = stats.get("database").unwrap_or(&stats);
    let disk_info = stats.get("disk").unwrap_or(&stats);
    (build_db_check(db_info), build_storage_check(disk_info))
}

// =============================================================================
/// Formats the `database` health block from `get_storage_stats()["database"]`.
fn build_db_check(db_info: &Value) -> Value {
    let db_bytes = db_info
        .get("db_size_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let wal_bytes = db_info
        .get("wal_size_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hist_cnt = db_info
        .get("history_records_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let ent_cnt = db_info
        .get("entities_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let row_counts = db_info
        .get("row_counts")
        .cloned()
        .unwrap_or_else(|| json!({ "entities": ent_cnt, "state_history": hist_cnt }));

    json!({
        "status": "pass",
        "attached": true,
        "db_size_mb": round_decimals(db_bytes as f64 / BYTES_PER_MIB, 2),
        "wal_size_mb": round_decimals(wal_bytes as f64 / BYTES_PER_MIB, 2),
        "db_size_bytes": db_bytes,
        "wal_size_bytes": wal_bytes,
        "total_size_human": db_info.get("total_size_human").and_then(Value::as_str).unwrap_or("0 B"),
        "history_records_count": hist_cnt,
        "entities_count": ent_cnt,
        "row_counts": row_counts,
        "oldest_record": db_info.get("oldest_record").cloned().unwrap_or(Value::Null),
        "newest_record": db_info.get("newest_record").cloned().unwrap_or(Value::Null),
    })
}

// =============================================================================
/// Formats the `storage` health block from `get_storage_stats()["disk"]`.
fn build_storage_check(disk_info: &Value) -> Value {
    let free_bytes = disk_info
        .get("free_bytes")
        .or_else(|| disk_info.get("disk_free_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let used_pct = disk_info
        .get("used_percent")
        .or_else(|| disk_info.get("disk_used_percent"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let status = if used_pct >= DISK_FAIL_PERCENT {
        "fail"
    } else if used_pct >= DISK_WARN_PERCENT {
        "warn"
    } else {
        "pass"
    };
    let free_human = disk_info
        .get("free_human")
        .or_else(|| disk_info.get("disk_free_human"))
        .and_then(Value::as_str)
        .unwrap_or("0 B");

    json!({
        "status": status,
        "free_gb": round_decimals(free_bytes as f64 / BYTES_PER_GIB, 2),
        "used_percent": used_pct,
        "disk_free_bytes": free_bytes,
        "disk_free_human": free_human,
        "disk_used_percent": used_pct,
    })
}

// =============================================================================
/// Queries `systemctl show` without emitting false warnings on uninstalled dev hosts.
fn query_systemd_service_state(service_name: &str) -> Value {
    let args = [
        "show",
        service_name,
        "--property=LoadState,ActiveState,SubState",
        "--no-pager",
    ];
    let is_systemd = std::env::var_os("INVOCATION_ID").is_some();
    let Ok(out) = run_command_captured("systemctl", &args, Duration::from_secs(5)) else {
        let st = if is_systemd { "warn" } else { "pass" };
        return json!({ "status": st, "active_state": "unknown" });
    };
    let mut load = String::new();
    let mut active = String::from("unknown");
    let mut sub = String::from("unknown");
    for line in out.stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "LoadState" => load = v.trim().to_owned(),
                "ActiveState" => active = v.trim().to_owned(),
                "SubState" => sub = v.trim().to_owned(),
                _ => {}
            }
        }
    }
    if load == "not-found" && !is_systemd {
        return json!({ "status": "pass", "active_state": "not_applicable", "sub_state": sub });
    }
    let status = match active.as_str() {
        "active" => "pass",
        "failed" => "fail",
        _ => "warn",
    };
    json!({ "status": status, "active_state": active, "sub_state": sub })
}

// =============================================================================
/// Builds `adapters` (`weather_service` & `rate_limiter`) health check block.
fn collect_adapter_checks(
    config: &AppConfig,
    weather: Option<&WeatherService>,
    rate_limiter: Option<&RateLimiter>,
) -> Value {
    let mut ws_state = weather.map_or_else(|| String::from("unknown"), WeatherService::get_status);
    if ws_state == "unknown"
        && weather
            .and_then(WeatherService::get_current_weather)
            .is_some()
    {
        ws_state = String::from("online");
    }
    let ws_status = if ws_state == "online" { "pass" } else { "warn" };
    let tracked = rate_limiter.map_or(0, RateLimiter::tracked_keys_count);

    json!({
        "weather_service": {
            "status": ws_status,
            "provider_state": ws_state,
        },
        "rate_limiter": {
            "enabled": config.rate_limit.enabled,
            "tracked_keys_count": tracked,
        },
    })
}

// =============================================================================
/// Derives active hardware alerts (`offline`, `battery_low`, `valve_error`, `fritz_offset`).
fn build_device_alerts(devices: &[Value]) -> Vec<Value> {
    let mut alerts = Vec::new();
    for dev in devices {
        let ain = dev.get("ain").and_then(Value::as_str).unwrap_or("");
        if ain.is_empty() || ain == WEATHER_AIN {
            continue;
        }
        let name = dev
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(ain);
        if dev.get("present").and_then(Value::as_bool) == Some(false) {
            alerts.push(json!({
                "severity": "warning",
                "category": "device",
                "code": ALERT_DEVICE_OFFLINE,
                "device": ain,
                "name": name,
                "message": format!("Device '{name}' ({ain}) is offline (not reachable via FRITZ!Box)"),
            }));
        }
        if dev.get("battery_low").and_then(Value::as_bool) == Some(true) {
            let bat = dev.get("battery_level").and_then(Value::as_i64);
            let sfx = bat.map_or_else(String::new, |b| format!(" ({b}%)"));
            alerts.push(json!({
                "severity": "warning",
                "category": "device",
                "code": ALERT_BATTERY_LOW,
                "device": ain,
                "name": name,
                "battery_level": bat,
                "message": format!("Device '{name}' ({ain}) reports low battery{sfx}"),
            }));
        }
        let err_code = dev.get("error_code").and_then(Value::as_u64).unwrap_or(0);
        if err_code > 0 {
            #[allow(clippy::cast_possible_truncation)]
            let desc = dev
                .get("error_description")
                .and_then(Value::as_str)
                .map_or_else(
                    || thermostat_error_description(err_code as u8).to_owned(),
                    str::to_owned,
                );
            alerts.push(json!({
                "severity": "error",
                "category": "device",
                "code": format!("{ALERT_VALVE_ERROR_PREFIX}{err_code}"),
                "device": ain,
                "name": name,
                "error_code": err_code,
                "error_description": desc,
                "message": format!("Thermostat '{name}' ({ain}) valve error {err_code}: {desc}"),
            }));
        }
        let dtype = dev.get("device_type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            dtype,
            device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
        ) {
            let f_off = dev
                .get("fritz_temperature_offset")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if f_off.abs() >= RULE_TEMP_EPSILON {
                alerts.push(json!({
                    "severity": "warning",
                    "category": "device",
                    "code": ALERT_FRITZ_OFFSET_ACTIVE,
                    "device": ain,
                    "name": name,
                    "fritz_temperature_offset": f_off,
                    "message": format!(
                        "Thermostat '{name}' ({ain}) has an active FRITZ!Box hardware offset \
                         ({f_off:+.1} °C); set FRITZ!Box offset to 0.0 °C to enable CowHome calibration"
                    ),
                }));
            }
        }
    }
    alerts
}
