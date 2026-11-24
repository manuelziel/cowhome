//! Break-glass emergency automation shutdown (`Emergency: Disable All Automations`).
//!
//! Scans `Global`, `Group`, and standalone `Device` scopes for active rules, prompts
//! for explicit confirmation, and disables all active automations via the live local
//! REST API (when running) or directly in SQLite (preserving saved schedules & groups).

use std::collections::HashSet;
use std::time::Duration;

use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};

use crate::cli::input::prompt_confirm;
use crate::cli::output::ConsolePrinter;
use crate::core::config::AppConfig;
use crate::core::constants::{
    cli_style::ANSI_CYAN,
    route_paths::{AUTOMATION, HEADER_AUTHORIZATION},
    service_defaults::{DEFAULT_SERVER_HOST, LOCAL_API_TIMEOUT_SECS},
};
use crate::core::domain_constants::{
    automation::automation_identifiers::{ALL_FEATURES, SCOPE_DEVICE, SCOPE_GLOBAL, SCOPE_GROUP},
    hardware::special_devices::GROUP_ID_PREFIX,
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::time_util::normalize_ain;
use crate::core::validation::is_api_key_secure;
use crate::db::Database;

/// Tuple `(scope_type, scope_id, display_label, active_feature_names)`.
type ActiveScopeEntry = (String, String, String, Vec<String>);

// =============================================================================
/// Runs the interactive `Emergency: Disable All Automations` confirmation workflow.
///
/// # Errors
/// Returns [`AppError`] on SQLite failure or unrecoverable terminal I/O error.
pub fn run_emergency_disable_automations(
    config: &AppConfig,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    printer.print_subsection("Emergency: Disable All Automations");
    let db_path = config.resolved_database_path();
    if !db_path.exists() {
        printer.print_info("Database not initialized — all Smart Heating automations are OFF.");
        return Ok(());
    }

    let db = Database::open(&db_path, &config.weather.timezone, logger.clone())?;
    let active_scopes = collect_active_scopes(&db)?;
    if active_scopes.is_empty() {
        db.close();
        printer.print_info("All Smart Heating automations are already OFF.");
        eprintln!("  (Configure room schedules and rules in the LibraHome app.)");
        return Ok(());
    }

    let total_rules = render_active_scope_summary(&active_scopes, printer);
    let confirmed = match prompt_confirm("Disable all active automations now?", false) {
        Ok(ans) => ans,
        Err(AppError::Cancelled) => false,
        Err(err) => {
            db.close();
            return Err(err);
        }
    };
    if !confirmed {
        db.close();
        printer.print_info("Operation cancelled. No changes were made.");
        return Ok(());
    }

    let res = disable_all_scopes(config, &db, &active_scopes);
    db.close();
    res?;
    log_and_report_break_glass(total_rules, active_scopes.len(), logger, printer);
    Ok(())
}

// =============================================================================
/// Prints all active scope labels & features, returning the total active rule count.
fn render_active_scope_summary(
    active_scopes: &[ActiveScopeEntry],
    printer: ConsolePrinter,
) -> usize {
    let total_rules: usize = active_scopes.iter().map(|(_, _, _, f)| f.len()).sum();
    eprintln!(
        "\nActive automations found across {} scope(s):\n",
        active_scopes.len()
    );
    for (_, _, label, feats) in active_scopes {
        let styled = printer.colorize(label, ANSI_CYAN);
        eprintln!("  • {styled}: {}", feats.join(", "));
    }
    eprintln!();
    printer.print_info(
        "Saved schedules, thresholds, and room groups are preserved and can be re-enabled in LibraHome.",
    );
    eprintln!();
    total_rules
}

// =============================================================================
/// Logs a warning to `journald` and emits a `✓` confirmation line on `stderr`.
fn log_and_report_break_glass(
    total_rules: usize,
    scope_cnt: usize,
    logger: &Logger,
    printer: ConsolePrinter,
) {
    logger.warning(&format!(
        "Emergency CLI break-glass disabled {total_rules} active automation(s) across {scope_cnt} scope(s)."
    ));
    printer.print_success(&format!(
        "Disabled {total_rules} active automation(s) across {scope_cnt} scope(s)."
    ));
}

// =============================================================================
/// Scans `global`, `group`, and ungrouped `device` scopes for active automation rules.
///
/// # Errors
/// Returns [`AppError`] if querying SQLite automation or group tables fails.
fn collect_active_scopes(db: &Database) -> AppResult<Vec<ActiveScopeEntry>> {
    let mut entries = Vec::new();
    let glob = db.get_all_automation_configs(SCOPE_GLOBAL, SCOPE_GLOBAL)?;
    let glob_feats = extract_active_features(&glob);
    if !glob_feats.is_empty() {
        entries.push((
            SCOPE_GLOBAL.to_owned(),
            SCOPE_GLOBAL.to_owned(),
            String::from("Global (House-wide)"),
            glob_feats,
        ));
    }
    let grouped_norms = append_group_scopes(db, &mut entries)?;
    append_standalone_device_scopes(db, &grouped_norms, &mut entries)?;
    Ok(entries)
}

// =============================================================================
/// Appends active `group` scopes to `entries` and returns all grouped normalized AINs.
///
/// # Errors
/// Returns [`AppError`] if querying `software_groups` fails.
fn append_group_scopes(
    db: &Database,
    entries: &mut Vec<ActiveScopeEntry>,
) -> AppResult<HashSet<String>> {
    let mut grouped_norms = HashSet::new();
    for grp in db.get_all_software_groups()? {
        let gid = grp.get("id").and_then(Value::as_str).unwrap_or("");
        let gname = grp.get("name").and_then(Value::as_str).unwrap_or(gid);
        if let Some(mems) = grp.get("members").and_then(Value::as_array) {
            grouped_norms.extend(mems.iter().filter_map(Value::as_str).map(normalize_ain));
        }
        if let Some(auto) = grp.get("automation") {
            let feats = extract_active_features(auto);
            if !feats.is_empty() {
                entries.push((
                    SCOPE_GROUP.to_owned(),
                    gid.to_owned(),
                    format!("Room '{gname}' ({GROUP_ID_PREFIX}{gid})"),
                    feats,
                ));
            }
        }
    }
    Ok(grouped_norms)
}

// =============================================================================
/// Appends active ungrouped `device` scopes to `entries`.
///
/// # Errors
/// Returns [`AppError`] if querying device-scoped automations or entities fails.
fn append_standalone_device_scopes(
    db: &Database,
    grouped_norms: &HashSet<String>,
    entries: &mut Vec<ActiveScopeEntry>,
) -> AppResult<()> {
    for (norm_ain, cfgs) in db.get_all_device_scope_configs()? {
        if grouped_norms.contains(&norm_ain) {
            continue;
        }
        let feats = extract_active_features(&cfgs);
        if !feats.is_empty() {
            let (ain, name) = resolve_cli_entity_identity(db, &norm_ain)?;
            entries.push((
                SCOPE_DEVICE.to_owned(),
                ain.clone(),
                format!("Device '{name}' ({ain})"),
                feats,
            ));
        }
    }
    Ok(())
}

// =============================================================================
/// Extracts ordered active feature identifiers from a scope JSON object.
fn extract_active_features(configs: &Value) -> Vec<String> {
    ALL_FEATURES
        .iter()
        .filter(|&&feat| {
            configs
                .get(feat)
                .and_then(|e| e.get("is_active"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|&s| s.to_owned())
        .collect()
}

// =============================================================================
/// Resolves `(canonical_ain, display_name)` from SQLite `entities` & `device_custom_names`.
///
/// # Errors
/// Returns [`AppError`] if querying `entities` or `device_custom_names` fails.
fn resolve_cli_entity_identity(db: &Database, norm_ain: &str) -> AppResult<(String, String)> {
    let row: Option<(String, String)> = db.with_conn(|conn| {
        conn.query_row(
            "SELECT ain, name FROM entities WHERE replace(ain, ' ', '') = ? LIMIT 1",
            [norm_ain],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(AppError::from)
    })?;
    let ain = row
        .as_ref()
        .map(|(a, _)| a.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(norm_ain)
        .to_owned();
    let custom = db.get_custom_name(&ain)?;
    let name = custom.unwrap_or_else(|| {
        row.map(|(_, n)| n)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ain.clone())
    });
    Ok((ain, name))
}

// =============================================================================
/// Disables all `active_scopes` via running local REST API or direct SQLite fallback.
///
/// # Errors
/// Returns [`AppError`] if initializing `tokio` or updating SQLite fails.
fn disable_all_scopes(
    config: &AppConfig,
    db: &Database,
    active_scopes: &[ActiveScopeEntry],
) -> AppResult<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| AppError::Service(format!("Failed initializing tokio runtime: {err}")))?;

    for (scope_type, scope_id, _, feats) in active_scopes {
        let mut batch = Map::with_capacity(feats.len());
        for feat in feats {
            batch.insert(feat.clone(), json!({ "is_active": false }));
        }
        let api_ok = rt.block_on(try_disable_via_local_api(
            config, scope_type, scope_id, &batch,
        ));
        if !api_ok {
            db.set_automation_configs_batch(&batch, scope_type, scope_id, None)?;
        }
    }
    Ok(())
}

// =============================================================================
/// Sends `PUT /api/v1/automation` to the local daemon so `RulesEngine` releases immediately.
async fn try_disable_via_local_api(
    config: &AppConfig,
    scope_type: &str,
    scope_id: &str,
    batch: &Map<String, Value>,
) -> bool {
    if !is_api_key_secure(&config.api.api_key) || config.server.port == 0 {
        return false;
    }
    let host = match config.server.host.as_str() {
        "0.0.0.0" | "::" => DEFAULT_SERVER_HOST,
        other => other,
    };
    let url = format!("http://{host}:{}{AUTOMATION}", config.server.port);
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(LOCAL_API_TIMEOUT_SECS))
        .build()
    else {
        return false;
    };
    let mut body = batch.clone();
    body.insert(String::from("scope_type"), json!(scope_type));
    body.insert(String::from("scope_id"), json!(scope_id));

    client
        .put(&url)
        .header(
            HEADER_AUTHORIZATION,
            format!("Bearer {}", config.api.api_key),
        )
        .json(&Value::Object(body))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}
