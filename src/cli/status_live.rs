//! Live FritzBox, Router, & Storage status probes (`cowhome status`).
//!
//! Queries the running local CowHome REST API (`/system/status`, `/system/storage`,
//! `/state`) when reachable, delegating device formatting to [`super::status_devices`],
//! or falls back gracefully to offline SQLite storage telemetry.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;

use crate::cli::output::ConsolePrinter;
use crate::cli::status_devices::{render_state_section, str_or_default};
use crate::core::config::AppConfig;
use crate::core::constants::{
    route_paths::{HEADER_AUTHORIZATION, STATE, SYSTEM_STATUS, SYSTEM_STORAGE},
    service_defaults::{DEFAULT_SERVER_HOST, LOCAL_API_TIMEOUT_SECS as STATUS_HTTP_TIMEOUT_SECS},
};
use crate::core::logger::Logger;
use crate::core::validation::is_api_key_secure;
use crate::db::Database;

// =============================================================================
/// Queries live local daemon endpoints (or offline SQLite) and renders domain telemetry.
pub fn render_live_cowhome_telemetry(config: &AppConfig, logger: &Logger, printer: ConsolePrinter) {
    if !is_api_key_secure(&config.api.api_key) {
        eprintln!();
        printer.print_warning(
            "API key is missing or still set to a default placeholder — live API telemetry skipped.",
        );
        render_offline_sqlite_summary(config, logger, printer);
        return;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    if rt.block_on(try_render_from_local_api(config, printer)) {
        return;
    }
    render_offline_sqlite_summary(config, logger, printer);
}

// =============================================================================
/// Fetches `/system/status`, `/system/storage`, and `/state` from the running daemon.
async fn try_render_from_local_api(config: &AppConfig, printer: ConsolePrinter) -> bool {
    let host = match config.server.host.as_str() {
        "0.0.0.0" | "::" => DEFAULT_SERVER_HOST,
        other => other,
    };
    let base = format!("http://{host}:{}", config.server.port);
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(STATUS_HTTP_TIMEOUT_SECS))
        .build()
    else {
        return false;
    };
    let auth = format!("Bearer {}", config.api.api_key);
    if !fetch_and_render_router(&client, &base, &auth, printer).await {
        return false;
    }
    fetch_and_render_endpoint(&client, &format!("{base}{SYSTEM_STORAGE}"), &auth, |d| {
        render_storage_details(d, printer);
    })
    .await;
    fetch_and_render_endpoint(&client, &format!("{base}{STATE}"), &auth, |d| {
        render_state_section(d, printer);
    })
    .await;
    true
}

// =============================================================================
/// Queries `/system/status` and reports actionable feedback when offline or `401`.
async fn fetch_and_render_router(
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    printer: ConsolePrinter,
) -> bool {
    let Ok(resp) = client
        .get(format!("{base}{SYSTEM_STATUS}"))
        .header(HEADER_AUTHORIZATION, auth)
        .send()
        .await
    else {
        eprintln!();
        printer.print_info(&format!(
            "CowHome daemon not reachable at {base} — showing offline SQLite telemetry."
        ));
        return false;
    };
    if resp.status() == StatusCode::UNAUTHORIZED {
        eprintln!();
        printer.print_error("Local API authentication failed (HTTP 401 — check api.api_key).");
        return false;
    }
    if !resp.status().is_success() {
        return false;
    }
    if let Ok(json) = resp.json::<Value>().await {
        if let Some(data) = json.get("data") {
            render_router_section(data, printer);
        }
    }
    true
}

// =============================================================================
/// Fetches a JSON `{ "data": ... }` endpoint and passes `data` to `render_fn`.
async fn fetch_and_render_endpoint<F>(client: &reqwest::Client, url: &str, auth: &str, render_fn: F)
where
    F: FnOnce(&Value),
{
    if let Ok(resp) = client
        .get(url)
        .header(HEADER_AUTHORIZATION, auth)
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<Value>().await {
                if let Some(data) = json.get("data") {
                    render_fn(data);
                }
            }
        }
    }
}

// =============================================================================
/// Renders offline SQLite storage metrics when the background daemon is not running.
fn render_offline_sqlite_summary(config: &AppConfig, logger: &Logger, printer: ConsolePrinter) {
    let db_path = config.resolved_database_path();
    if !db_path.exists() {
        return;
    }
    if let Ok(db) = Database::open(&db_path, &config.weather.timezone, logger.clone()) {
        if let Ok(stats) = db.get_storage_stats() {
            render_storage_details(&stats, printer);
        }
        db.close();
    }
}

// =============================================================================
/// Renders the *FritzBox & Router* section (`ONLINE`/`OFFLINE`, Model, WAN IP, WLAN).
fn render_router_section(data: &Value, printer: ConsolePrinter) {
    printer.print_subsection("FritzBox & Router");
    let conn = data.get("connection_status");
    let info = data.get("router_info");
    let has_info = info
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());
    let online = has_info || is_wan_online(conn);
    if online {
        printer.print_success("FritzBox Connection: ONLINE");
    } else {
        printer.print_error("FritzBox Connection: OFFLINE");
    }
    let wan_ip = conn
        .and_then(|c| c.get("external_ip").and_then(Value::as_str))
        .or_else(|| data.get("wan_ip").and_then(Value::as_str))
        .unwrap_or("Unknown");
    if let Some(info_val) = info.filter(|_| has_info) {
        eprintln!(
            "  Router Model: {}",
            str_or_default(info_val, "model", "Unknown")
        );
        eprintln!(
            "  Firmware:     {}",
            str_or_default(info_val, "software_version", "Unknown")
        );
        eprintln!("  WAN IP:       {wan_ip}");
    }
    eprintln!(
        "  WLAN (Main):  {}",
        format_wlan_label(data.get("wlan_main"))
    );
    eprintln!(
        "  WLAN (Guest): {}",
        format_wlan_label(data.get("wlan_guest"))
    );
}

// =============================================================================
/// Checks `is_connected` / `is_linked` flags on `connection_status`.
fn is_wan_online(conn: Option<&Value>) -> bool {
    conn.and_then(|c| {
        c.get("is_connected")
            .and_then(Value::as_bool)
            .or_else(|| c.get("is_linked").and_then(Value::as_bool))
    })
    .unwrap_or(false)
}

// =============================================================================
/// Formats a WLAN status JSON block into `"ON (SSID)"`, `"ON"`, or `"OFF"`.
fn format_wlan_label(wlan: Option<&Value>) -> String {
    let Some(w) = wlan else {
        return String::from("OFF");
    };
    if let Some(obj) = w.as_object() {
        if !obj.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
            return String::from("OFF");
        }
        if let Some(ssid) = obj
            .get("ssid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return format!("ON ({ssid})");
        }
        return String::from("ON");
    }
    if w.as_bool().unwrap_or(false) {
        String::from("ON")
    } else {
        String::from("OFF")
    }
}

// =============================================================================
/// Renders detailed `.db` / `-wal` / `-shm` sizes, row counts, and history range.
fn render_storage_details(payload: &Value, printer: ConsolePrinter) {
    printer.print_subsection("Storage & Database Telemetry");
    let db = payload.get("database").unwrap_or(payload);
    let disk = payload.get("disk").unwrap_or(payload);
    let records = db
        .get("history_records_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let entities = db
        .get("entities_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let used_pct = disk
        .get("used_percent")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    eprintln!(
        "  Database Size:   {} (WAL: {}, SHM: {}, Total: {})",
        str_or_default(db, "db_size_human", "?"),
        str_or_default(db, "wal_size_human", "?"),
        str_or_default(db, "shm_size_human", "?"),
        str_or_default(db, "total_size_human", "?"),
    );
    eprintln!("  Data Points:     {records} across {entities} entities");
    eprintln!(
        "  Partition Usage: {} free of {} ({used_pct:.1}% used)",
        str_or_default(disk, "free_human", "?"),
        str_or_default(disk, "total_human", "?"),
    );
    if let (Some(old), Some(new)) = (
        db.get("oldest_record").and_then(Value::as_str),
        db.get("newest_record").and_then(Value::as_str),
    ) {
        eprintln!("  History Range:   {old} -> {new}");
    }
}
