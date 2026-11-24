//! Smart Energy metering (`DIN EN IEC 62053-21`), tariffs (`EnWG § 40`), & grid-meter priority.
//!
//! Stores electricity working price (`EUR/kWh`) and monthly base fee (`EUR/month`) in
//! `system_settings` with LWW protection, integrates 365-day switch/meter history, and
//! prioritizes dedicated optical main grid meters (`FRITZ!Smart Energy 250`) on house totals.

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::{Map, Value};

use super::Database;
use super::entities_sync::deduplicate_ains_by_norm;
use super::settings::NumericSettingSpec;
use crate::core::domain_constants::{
    analytics::{
        efficiency_defaults::SECONDS_PER_DAY,
        energy_defaults::{
            DEFAULT_BASE_FEE_EUR_PER_MONTH, DEFAULT_PRICE_EUR_PER_KWH, GRID_METER_PRODUCT_HINT,
            IDENTIFIER_HOUSE, MAX_BASE_FEE_EUR_PER_MONTH, MAX_PRICE_EUR_PER_KWH,
            MIN_BASE_FEE_EUR_PER_MONTH, MIN_PRICE_EUR_PER_KWH, SETTING_KEY_BASE_FEE,
            SETTING_KEY_PRICE, YEAR_DAYS,
        },
    },
    automation::weather_defaults::{TIMEZONE, TIMEZONE_AUTO},
    hardware::{
        device_capabilities, device_types, fritz_api::SUBUNIT_DEFAULT_SUFFIX,
        special_devices::GROUP_ID_PREFIX,
    },
};
use crate::core::energy_analytics::{
    EnergySample, EnergySummary, aggregate_energy_summaries, build_device_energy_summary,
};
use crate::core::error::AppResult;
use crate::core::time_util::{normalize_ain, now_epoch_f64};
use crate::models::group::format_group_id;
use crate::models::registry::LiveSwitchTelemetry;

impl Database {
    // =========================================================================
    /// Reads the configured working electricity tariff (`EUR/kWh`, default `0.35`).
    #[must_use]
    pub fn get_electricity_price(&self) -> f64 {
        self.get_numeric_setting(
            SETTING_KEY_PRICE,
            DEFAULT_PRICE_EUR_PER_KWH,
            MIN_PRICE_EUR_PER_KWH,
            MAX_PRICE_EUR_PER_KWH,
            4,
        )
    }

    // =========================================================================
    /// Validates (`0.0..=5.0 EUR/kWh`) and persists the electricity price with LWW protection.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`](crate::core::error::AppError::Validation) or
    /// [`AppError::Database`](crate::core::error::AppError::Database).
    pub fn set_electricity_price(
        &self,
        raw_price: &Value,
        effective_ts: Option<f64>,
    ) -> AppResult<(f64, bool)> {
        self.set_numeric_setting(
            NumericSettingSpec {
                key: SETTING_KEY_PRICE,
                default: DEFAULT_PRICE_EUR_PER_KWH,
                min_val: MIN_PRICE_EUR_PER_KWH,
                max_val: MAX_PRICE_EUR_PER_KWH,
                decimals: 4,
            },
            raw_price,
            effective_ts,
        )
    }

    // =========================================================================
    /// Reads the configured monthly electricity base fee (`EUR/month`, default `0.00`).
    #[must_use]
    pub fn get_electricity_base_fee(&self) -> f64 {
        self.get_numeric_setting(
            SETTING_KEY_BASE_FEE,
            DEFAULT_BASE_FEE_EUR_PER_MONTH,
            MIN_BASE_FEE_EUR_PER_MONTH,
            MAX_BASE_FEE_EUR_PER_MONTH,
            2,
        )
    }

    // =========================================================================
    /// Validates (`0.0..=500.0 EUR/month`) and persists the monthly base fee with LWW protection.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`](crate::core::error::AppError::Validation) or
    /// [`AppError::Database`](crate::core::error::AppError::Database).
    pub fn set_electricity_base_fee(
        &self,
        raw_fee: &Value,
        effective_ts: Option<f64>,
    ) -> AppResult<(f64, bool)> {
        self.set_numeric_setting(
            NumericSettingSpec {
                key: SETTING_KEY_BASE_FEE,
                default: DEFAULT_BASE_FEE_EUR_PER_MONTH,
                min_val: MIN_BASE_FEE_EUR_PER_MONTH,
                max_val: MAX_BASE_FEE_EUR_PER_MONTH,
                decimals: 2,
            },
            raw_fee,
            effective_ts,
        )
    }

    // =========================================================================
    /// Computes the [`EnergySummary`] (today, 7d, 30d, 365d + forecast) for a switch or group.
    #[must_use]
    pub fn get_energy_consumption_summary(
        &self,
        ain: &str,
        tz_name: Option<&str>,
        now_ts: Option<f64>,
        live_switches: Option<&HashMap<String, LiveSwitchTelemetry>>,
    ) -> Option<EnergySummary> {
        let trimmed = ain.trim();
        if trimmed.starts_with(GROUP_ID_PREFIX) {
            let grp = self.get_software_group(trimmed).ok()??;
            let gid_num = grp.get("id")?.as_str()?.parse::<i64>().ok()?;
            let members: Vec<String> = grp
                .get("members")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            return self.aggregate_energy_for_ains(
                &members,
                &format_group_id(gid_num),
                tz_name,
                now_ts,
                live_switches,
            );
        }
        let entity = self.resolve_entity(trimmed).ok()?;
        if entity.device_type != device_types::SWITCH {
            return None;
        }
        let eff_now = now_ts.unwrap_or_else(now_epoch_f64);
        let start_365d = eff_now - YEAR_DAYS * SECONDS_PER_DAY;
        let rows = self
            .fetch_bounded_history_rows(entity.entity_id, start_365d, eff_now)
            .ok()?;
        let mut samples: Vec<EnergySample> = rows
            .into_iter()
            .map(|(ts, map)| map_to_energy_sample(ts, &map))
            .collect();

        let norm = normalize_ain(&entity.ain);
        if let Some(live) = live_switches.and_then(|m| m.get(&norm))
            && samples.last().is_none_or(|last| eff_now >= last.timestamp)
        {
            samples.push(EnergySample {
                timestamp: eff_now,
                present: live.present,
                power_current_w: Some(live.power_current),
                energy_total_wh: Some(live.energy_total),
            });
        }
        let eff_tz = self.resolve_home_tz_name(tz_name);
        Some(build_device_energy_summary(
            &entity.ain,
            &samples,
            self.get_electricity_price(),
            self.get_electricity_base_fee(),
            Some(eff_now),
            Some(&eff_tz),
        ))
    }

    // =========================================================================
    /// Aggregates [`EnergySummary`] objects across `ains` (`identifier = "house"` or `"group-*"`).
    #[must_use]
    pub fn aggregate_energy_for_ains(
        &self,
        ains: &[String],
        identifier: &str,
        tz_name: Option<&str>,
        now_ts: Option<f64>,
        live_switches: Option<&HashMap<String, LiveSwitchTelemetry>>,
    ) -> Option<EnergySummary> {
        let items: Vec<EnergySummary> = ains
            .iter()
            .filter_map(|a| self.get_energy_consumption_summary(a, tz_name, now_ts, live_switches))
            .collect();
        aggregate_energy_summaries(
            &items,
            self.get_electricity_price(),
            self.get_electricity_base_fee(),
            identifier,
        )
    }

    // =========================================================================
    /// Computes the house-wide [`EnergySummary`], prioritizing non-switchable optical grid
    /// meters (`FRITZ!Smart Energy 250`) over downstream smart plugs (`200/210`).
    #[must_use]
    pub fn get_house_energy_summary(
        &self,
        tz_name: Option<&str>,
        now_ts: Option<f64>,
        live_switches: Option<&HashMap<String, LiveSwitchTelemetry>>,
    ) -> Option<EnergySummary> {
        let raw_ains = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT ain FROM entities WHERE device_type = ?")?;
                let rows = stmt
                    .query_map(params![device_types::SWITCH], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .ok()?;
        let ains = deduplicate_ains_by_norm(raw_ains);
        let norm_set: HashSet<String> = ains.iter().map(|a| normalize_ain(a)).collect();
        let filtered: Vec<String> = ains
            .into_iter()
            .filter(|a| {
                !norm_set.contains(&format!("{}{SUBUNIT_DEFAULT_SUFFIX}", normalize_ain(a)))
            })
            .collect();
        if filtered.is_empty() {
            return None;
        }
        let grid_meters: Vec<String> = filtered
            .iter()
            .filter(|a| self.is_grid_meter_device(a, live_switches))
            .cloned()
            .collect();
        if !grid_meters.is_empty()
            && let Some(grid_res) = self.aggregate_energy_for_ains(
                &grid_meters,
                IDENTIFIER_HOUSE,
                tz_name,
                now_ts,
                live_switches,
            )
        {
            return Some(grid_res);
        }
        self.aggregate_energy_for_ains(&filtered, IDENTIFIER_HOUSE, tz_name, now_ts, live_switches)
    }

    // =========================================================================
    /// Resolves the effective IANA home timezone name (`Europe/Berlin` fallback).
    fn resolve_home_tz_name(&self, explicit: Option<&str>) -> String {
        if let Some(tz) = explicit
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(TIMEZONE_AUTO))
        {
            return tz.to_owned();
        }
        let cfg_tz = self.default_timezone();
        if !cfg_tz.trim().is_empty() && !cfg_tz.eq_ignore_ascii_case(TIMEZONE_AUTO) {
            return cfg_tz;
        }
        TIMEZONE.to_owned()
    }

    // =========================================================================
    /// Detects dedicated optical grid meters (`power_meter` without `switch`, or product `"250"`).
    fn is_grid_meter_device(
        &self,
        ain: &str,
        live_switches: Option<&HashMap<String, LiveSwitchTelemetry>>,
    ) -> bool {
        let norm = normalize_ain(ain);
        if let Some(live) = live_switches.and_then(|m| m.get(&norm)) {
            if is_grid_meter_meta(&live.capabilities, live.product_name.as_deref()) {
                return true;
            }
            if !live.capabilities.is_empty() || live.product_name.is_some() {
                return false;
            }
        }
        let Some((snap, _)) = self.get_latest_state_snapshot(ain).ok().flatten() else {
            return false;
        };
        let caps: Vec<String> = snap
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let prod = snap.get("product_name").and_then(Value::as_str);
        is_grid_meter_meta(&caps, prod)
    }
}

// =============================================================================
/// Checks capabilities and product name for dedicated electricity-meter markers.
fn is_grid_meter_meta(capabilities: &[String], product_name: Option<&str>) -> bool {
    if !capabilities.is_empty() {
        let has_meter = capabilities
            .iter()
            .any(|c| c == device_capabilities::POWER_METER);
        let has_relay = capabilities
            .iter()
            .any(|c| c == device_capabilities::SWITCH);
        if has_meter && !has_relay {
            return true;
        }
    }
    product_name.is_some_and(|p| p.contains(GRID_METER_PRODUCT_HINT))
}

// =============================================================================
/// Converts a JSON `state_history` map into a typed [`EnergySample`].
fn map_to_energy_sample(timestamp: f64, map: &Map<String, Value>) -> EnergySample {
    EnergySample {
        timestamp,
        present: map.get("present") != Some(&Value::Bool(false)),
        power_current_w: map.get("power_current").and_then(Value::as_f64),
        energy_total_wh: map.get("energy_total").and_then(Value::as_f64),
    }
}
