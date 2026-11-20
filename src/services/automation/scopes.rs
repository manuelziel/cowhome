//! Single-Authority scope enrichment, group partitioning, & preset synchronization.
//!
//! Manages Digital Twin reference-sensor resets, standalone calibration restores,
//! automatic `FRITZ!Smart Control 440` group sensor binding, and comfort/eco preset sync.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::actuation::RULE_TEMP_EPSILON;
use super::apparent_temp::ApparentTemperatureRule;
pub use super::scope_binding::{
    bind_configs_to_targets, merge_global_fallbacks, parse_feature_cfg, resolve_preheat_state,
    resolve_schedule_transition, resolve_scheduled_base_temp, ScopeConfigMap,
};
use crate::core::domain_constants::{
    automation::{
        automation_defaults::{
            HUMIDITY_HYSTERESIS_PERCENT, HUMIDITY_THRESHOLD, MAX_DYNAMIC_TEMP_OFFSET,
        },
        automation_identifiers::{
            APPARENT_TEMPERATURE, HEATING_SCHEDULE, KEY_COMFORT_TEMP, KEY_ECO_TEMP, SCOPE_GROUP,
        },
    },
    hardware::{device_capabilities, device_types},
};
use crate::core::logger::Logger;
use crate::core::psychrometrics::{compute_device_psychrometrics, DevicePsychrometricsInput};
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;
use crate::services::weather::WeatherService;

// =============================================================================
/// Resets `reference_sensor_ain` / `dew_point_c` and refreshes static calibration flags.
pub async fn clear_reference_sensors(registry: &StateRegistry, db: &Database) {
    let devices = registry.get_all_devices();
    let thermo_ains: Vec<String> = devices
        .iter()
        .filter(|d| {
            matches!(
                d.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            )
        })
        .map(|d| d.ain.clone())
        .collect();
    let offsets = db
        .run_blocking(move |d| {
            Ok(thermo_ains
                .into_iter()
                .map(|a| {
                    let off = d.get_thermostat_offset(&a);
                    (a, off)
                })
                .collect::<Vec<_>>())
        })
        .await
        .unwrap_or_default();
    let offset_map: HashMap<String, f64> = offsets.into_iter().collect();

    for dev in devices {
        let off_opt = offset_map.get(&dev.ain).copied();
        registry.mutate_device(&dev.ain, |d| {
            d.dew_point_c = None;
            let present = d.present;
            let locked = d.automation_locked;
            if let Some(t) = d.as_thermostat_mut() {
                t.reference_sensor_ain = None;
                if let Some(off) = off_opt {
                    t.temperature_offset = off;
                }
                t.offset_control_active = present
                    && !t.window_open
                    && !t.boost_active
                    && !locked
                    && t.fritz_temperature_offset.abs() < RULE_TEMP_EPSILON
                    && t.temperature_offset.abs() >= RULE_TEMP_EPSILON;
            }
        });
    }
}

// =============================================================================
/// Applies static calibration on unlinked thermostats and enriches standalone sensors.
pub async fn restore_unlinked_thermostats(
    app_rule: &ApparentTemperatureRule,
    registry: &StateRegistry,
    weather_service: Option<&WeatherService>,
    aha: &FritzAhaClient,
) {
    let (temp_out, abs_out) = weather_service
        .and_then(WeatherService::get_current_weather)
        .map_or((None, None), |cw| cw.outdoor_psychrometrics());

    for dev in registry.get_all_devices() {
        let unlinked = dev
            .as_thermostat()
            .is_some_and(|t| t.reference_sensor_ain.is_none());
        if unlinked {
            app_rule.apply_static_calibration(&dev, registry, aha).await;
        }
        let refreshed = registry.get_device(&dev.ain).unwrap_or(dev);
        if refreshed.humidity.is_some() && refreshed.dew_point_c.is_none() {
            let temp_in = refreshed
                .as_thermostat()
                .map(|t| t.temperature_current)
                .or_else(|| refreshed.as_sensor().and_then(|s| s.temperature_current));
            let psych = compute_device_psychrometrics(&DevicePsychrometricsInput {
                temp_in_c: temp_in,
                rel_humidity_in_pct: refreshed.humidity,
                fallback_target_c: refreshed.as_thermostat().map(|t| t.temperature_target),
                temp_base_c: None,
                temp_out_c: temp_out,
                abs_out_gm3: abs_out,
                humidity_threshold: HUMIDITY_THRESHOLD,
                hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
                guard_active: refreshed.humidity_guard_active,
                rule_active: false,
            });
            registry.mutate_device(&refreshed.ain, |d| d.apply_psychrometrics(psych));
        }
    }
}

// =============================================================================
/// Separates group member AINs into `(controllable_targets, detected_sensor_ain)`.
#[must_use]
pub fn partition_group_members(
    members: &[String],
    registry: &StateRegistry,
) -> (Vec<String>, String) {
    let mut targets = Vec::new();
    let mut sensor_ain = String::new();
    for ain in members {
        let dev = registry.get_device(ain);
        let is_climate_sensor = dev.as_ref().is_some_and(|d| {
            d.as_sensor().is_some()
                || (d.device_type == device_types::SENSOR
                    && d.capabilities
                        .iter()
                        .any(|c| c == device_capabilities::TEMPERATURE_SENSOR))
        });
        if is_climate_sensor {
            if sensor_ain.is_empty() {
                if let Some(d) = dev {
                    sensor_ain = d.ain;
                }
            }
        } else if dev.as_ref().is_none_or(|d| {
            matches!(
                d.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP | device_types::SWITCH
            )
        }) {
            targets.push(ain.clone());
        }
    }
    (targets, sensor_ain)
}

// =============================================================================
/// Auto-binds and activates `ApparentTemperature` when a group contains a room sensor.
pub async fn ensure_group_sensor_binding(
    gid: &str,
    grp_configs: &mut ScopeConfigMap,
    group_sensor: &str,
    db: &Database,
    logger: &Logger,
) {
    if group_sensor.is_empty() || gid.is_empty() {
        return;
    }
    let app_entry = grp_configs.get(APPARENT_TEMPERATURE);
    let mut app_cfg = app_entry
        .and_then(|e| e.get("config"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let existing = app_cfg
        .get("sensor_ain")
        .and_then(Value::as_str)
        .unwrap_or("");
    if normalize_ain(existing) == normalize_ain(group_sensor) {
        return;
    }
    let was_empty = app_cfg.is_empty();
    app_cfg.insert(String::from("sensor_ain"), json!(group_sensor));
    app_cfg
        .entry(String::from("active_offset_control"))
        .or_insert_with(|| json!(true));
    app_cfg
        .entry(String::from("max_offset"))
        .or_insert_with(|| json!(MAX_DYNAMIC_TEMP_OFFSET));
    let is_active = if was_empty {
        true
    } else {
        app_entry
            .and_then(|e| e.get("is_active"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    let cfg_val = Value::Object(app_cfg);
    let gid_owned = gid.to_owned();
    let persist_val = cfg_val.clone();
    if let Err(err) = db
        .run_blocking(move |d| {
            d.set_automation_config(
                APPARENT_TEMPERATURE,
                Some(is_active),
                Some(&persist_val),
                SCOPE_GROUP,
                &gid_owned,
                None,
            )
        })
        .await
    {
        logger.warning(&format!(
            "Failed persisting auto sensor binding for group '{gid}': {err}"
        ));
    }
    grp_configs.insert(
        APPARENT_TEMPERATURE.to_owned(),
        json!({ "is_active": is_active, "config": cfg_val }),
    );
}

// =============================================================================
/// Synchronizes SQLite `(comfort_temp, eco_temp)` presets onto Digital Twin & `HeatingSchedule`.
pub async fn sync_scope_thermostat_presets(
    targets: &[String],
    configs: &mut ScopeConfigMap,
    registry: &StateRegistry,
    db: &Database,
) {
    if targets.is_empty() {
        return;
    }
    let ains = targets.to_vec();
    let presets = db
        .run_blocking(move |d| {
            Ok(ains
                .into_iter()
                .map(|a| {
                    let p = d.get_thermostat_presets(&a);
                    (a, p)
                })
                .collect::<Vec<_>>())
        })
        .await
        .unwrap_or_default();

    for (ain, (comfort, eco)) in &presets {
        registry.mutate_device(ain, |d| {
            if let Some(t) = d.as_thermostat_mut() {
                t.comfort_temperature = Some(*comfort);
                t.eco_temperature = Some(*eco);
            }
        });
    }
    if let Some((_, (comfort, eco))) = presets.first() {
        let (comfort, eco) = (*comfort, *eco);
        if let Some(sched_cfg) = configs
            .get_mut(HEATING_SCHEDULE)
            .and_then(Value::as_object_mut)
            .and_then(|m| m.get_mut("config"))
            .and_then(Value::as_object_mut)
        {
            sched_cfg
                .entry(String::from(KEY_COMFORT_TEMP))
                .or_insert_with(|| json!(comfort));
            sched_cfg
                .entry(String::from(KEY_ECO_TEMP))
                .or_insert_with(|| json!(eco));
        }
    }
}
