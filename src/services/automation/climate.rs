//! Seasonal and vacation guard rules (`VacationMode` & `WeatherSmartHeating`).
//!
//! Implements house-wide `Frost Guard` (`VacationModeRule`) with pre-return wakeup
//! and seasonal `Summer Pause` (`WeatherSmartHeatingRule`) with cold-snap exception.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::{Value, json};

use super::actuation::apply_twin_thermostat_target;
use crate::core::domain_constants::{
    automation::{
        automation_defaults::FROST_PROTECTION_TEMP,
        automation_identifiers::{SCOPE_GLOBAL, VACATION_MODE},
    },
    hardware::{device_types, special_devices::GROUP_ID_PREFIX},
};
use crate::core::heating_forecast::is_date_within_window;
use crate::core::logger::Logger;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::db::Database;
use crate::models::automation::{VacationModeConfig, WeatherSmartHeatingConfig};
use crate::models::registry::StateRegistry;
use crate::services::fritz::FritzAhaClient;
use crate::services::weather::WeatherService;

// =============================================================================
/// House-wide `VacationMode` (`Frost Guard`) rule with pre-return wakeup.
#[derive(Debug)]
pub struct VacationModeRule {
    /// Normalized AINs currently locked to frost protection.
    pub locked_ains: HashSet<String>,
    /// Application logger.
    logger: Logger,
}

impl VacationModeRule {
    /// Creates a new [`VacationModeRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            locked_ains: HashSet::new(),
            logger,
        }
    }

    /// Evaluates `VacationMode`, auto-disabling on return and enforcing frost protection.
    pub async fn evaluate(
        &mut self,
        config: &VacationModeConfig,
        registry: &StateRegistry,
        aha: &FritzAhaClient,
        db: &Database,
    ) {
        self.locked_ains.clear();
        let now = now_epoch_f64();
        if config.end_date_timestamp > 0.0 && now >= config.end_date_timestamp {
            self.logger
                .info("Vacation period ended. Automatically disabling VacationMode.");
            if let Err(err) = db
                .run_blocking(|d| {
                    d.set_automation_config(
                        VACATION_MODE,
                        Some(false),
                        None,
                        SCOPE_GLOBAL,
                        SCOPE_GLOBAL,
                        None,
                    )
                })
                .await
            {
                self.logger.warning(&format!(
                    "Failed persisting automatic VacationMode expiration: {err}"
                ));
            }
            return;
        }
        let pre_heat_start = config.end_date_timestamp - (config.pre_heat_hours as f64) * 3600.0;
        if config.end_date_timestamp > 0.0 && now >= pre_heat_start {
            return;
        }
        let exempt = resolve_vacation_exempt_ains(config, registry, db).await;
        for dev in registry.get_all_devices() {
            let norm = normalize_ain(&dev.ain);
            let is_thermo = matches!(
                dev.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            );
            if !is_thermo || exempt.contains(&norm) {
                continue;
            }
            self.locked_ains.insert(norm);
            let cur_t = dev.as_thermostat().map_or(0.0, |t| t.temperature_target);
            if cur_t > config.frost_protection_temp {
                self.logger.info(&format!(
                    "Vacation Mode: Enforcing {}°C on {}.",
                    config.frost_protection_temp, dev.name
                ));
                if aha
                    .set_target_temperature(&dev.ain, config.frost_protection_temp)
                    .await
                    .is_ok()
                {
                    apply_twin_thermostat_target(registry, &dev.ain, config.frost_protection_temp);
                }
            }
        }
    }
}

// =============================================================================
/// Seasonal `WeatherSmartHeating` (`Summer Pause`) rule with cold-snap exception.
#[derive(Debug)]
pub struct WeatherSmartHeatingRule {
    /// Normalized AINs suppressed during the current evaluation cycle.
    pub last_suppressed_ains: HashSet<String>,
    /// Pre-suppression target temperatures saved per normalized AIN.
    pre_suppressed_targets: HashMap<String, f64>,
    /// Application logger.
    logger: Logger,
}

impl WeatherSmartHeatingRule {
    /// Creates a new [`WeatherSmartHeatingRule`].
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            last_suppressed_ains: HashSet::new(),
            pre_suppressed_targets: HashMap::new(),
            logger,
        }
    }

    /// Evaluates `Summer Pause` across `config.affected_devices`.
    pub async fn evaluate(
        &mut self,
        config: &WeatherSmartHeatingConfig,
        registry: &StateRegistry,
        weather_service: Option<&WeatherService>,
        aha: &FritzAhaClient,
        now_dt: DateTime<Tz>,
    ) {
        if config.affected_devices.is_empty() {
            return;
        }
        for ain in &config.affected_devices {
            self.last_suppressed_ains.remove(&normalize_ain(ain));
        }
        let md = now_dt.format("%m-%d").to_string();
        if !is_date_within_window(&md, &config.start_date, &config.end_date) {
            self.restore_devices(&config.affected_devices, registry, aha)
                .await;
            return;
        }
        let Some(max_out) = peak_outdoor_temp(weather_service) else {
            return;
        };
        if max_out >= config.outdoor_temp_threshold {
            self.logger.info(&format!(
                "Outdoor/forecast temp ({max_out}°C) >= threshold ({}°C), suppressing heating.",
                config.outdoor_temp_threshold
            ));
            self.suppress_devices(&config.affected_devices, registry, aha)
                .await;
        } else {
            self.restore_devices(&config.affected_devices, registry, aha)
                .await;
        }
    }

    /// Restores pre-suppression setpoints when `Summer Pause` releases heating.
    pub async fn restore_devices(
        &mut self,
        devices: &[String],
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        for ain in devices {
            let norm = normalize_ain(ain);
            let Some(restore_temp) = self.pre_suppressed_targets.remove(&norm) else {
                continue;
            };
            let Some(dev) = registry.get_device(ain) else {
                continue;
            };
            let cur_t = dev.as_thermostat().map_or(0.0, |t| t.temperature_target);
            if cur_t <= FROST_PROTECTION_TEMP {
                self.logger.info(&format!(
                    "Summer Pause released on {}, restoring {restore_temp}°C.",
                    dev.name
                ));
                if aha
                    .set_target_temperature(&dev.ain, restore_temp)
                    .await
                    .is_ok()
                {
                    apply_twin_thermostat_target(registry, &dev.ain, restore_temp);
                }
            }
        }
    }

    /// Builds the `WeatherSmartHeating` diagnostic decision trace JSON object.
    #[must_use]
    pub fn describe_scope_trace(
        &self,
        config: &WeatherSmartHeatingConfig,
        is_active: bool,
        weather_service: Option<&WeatherService>,
        now_dt: DateTime<Tz>,
    ) -> Value {
        let md = now_dt.format("%m-%d").to_string();
        let in_win = is_date_within_window(&md, &config.start_date, &config.end_date);
        json!({
            "is_active": is_active,
            "current_date_md": md,
            "start_date": config.start_date,
            "end_date": config.end_date,
            "in_season_window": in_win,
            "max_outdoor_temp": peak_outdoor_temp(weather_service),
            "outdoor_temp_threshold": config.outdoor_temp_threshold,
        })
    }

    /// Lowers target temperature to `FROST_PROTECTION_TEMP` (`8.0 °C`) across `devices`.
    async fn suppress_devices(
        &mut self,
        devices: &[String],
        registry: &StateRegistry,
        aha: &FritzAhaClient,
    ) {
        for ain in devices {
            let Some(dev) = registry.get_device(ain) else {
                continue;
            };
            let is_thermo = matches!(
                dev.device_type.as_str(),
                device_types::THERMOSTAT | device_types::THERMOSTAT_GROUP
            );
            if !is_thermo {
                continue;
            }
            let norm = normalize_ain(&dev.ain);
            self.last_suppressed_ains.insert(norm.clone());
            let cur_t = dev.as_thermostat().map_or(0.0, |t| t.temperature_target);
            if cur_t > FROST_PROTECTION_TEMP {
                self.pre_suppressed_targets.entry(norm).or_insert(cur_t);
                self.logger.info(&format!(
                    "Suppressing heating on {} to {FROST_PROTECTION_TEMP}°C.",
                    dev.name
                ));
                if aha
                    .set_target_temperature(&dev.ain, FROST_PROTECTION_TEMP)
                    .await
                    .is_ok()
                {
                    apply_twin_thermostat_target(registry, &dev.ain, FROST_PROTECTION_TEMP);
                }
            }
        }
    }
}

// =============================================================================
/// Resolves peak outdoor temperature across `current` and upcoming 2h `hourly` forecast slots.
fn peak_outdoor_temp(weather_service: Option<&WeatherService>) -> Option<f64> {
    let snap = weather_service?.get_snapshot();
    let cur = snap.current.as_ref()?;
    let mut peak = cur.apparent_temperature.or(cur.temperature)?;
    for slot in &snap.hourly {
        if let Some(t) = slot.temperature {
            peak = peak.max(t);
        }
    }
    Some(peak)
}

// =============================================================================
/// Resolves normalized AINs exempt from `VacationMode` via `ignored_devices` or `ignored_groups`.
async fn resolve_vacation_exempt_ains(
    config: &VacationModeConfig,
    registry: &StateRegistry,
    db: &Database,
) -> HashSet<String> {
    let mut exempt: HashSet<String> = config
        .ignored_devices
        .iter()
        .map(|a| normalize_ain(a))
        .filter(|s| !s.is_empty())
        .collect();
    let ignored_gids: HashSet<String> = config
        .ignored_groups
        .iter()
        .map(|g| g.trim().trim_start_matches(GROUP_ID_PREFIX).to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if ignored_gids.is_empty() {
        return exempt;
    }
    for dev in registry.get_all_devices() {
        if let Some(ref gid) = dev.group_id
            && ignored_gids.contains(gid.trim_start_matches(GROUP_ID_PREFIX))
        {
            exempt.insert(normalize_ain(&dev.ain));
        }
    }
    if let Ok(groups) = db.run_blocking(|d| d.get_all_software_groups()).await {
        for grp in groups {
            if grp
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| ignored_gids.contains(id))
                && let Some(arr) = grp.get("members").and_then(Value::as_array)
            {
                exempt.extend(arr.iter().filter_map(Value::as_str).map(normalize_ain));
            }
        }
    }
    exempt
}
