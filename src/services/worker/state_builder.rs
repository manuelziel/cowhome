//! Digital Twin device state builders and software-group Single-Authority mapping.
//!
//! Pure transformation functions converting [`RawAhaDevice`] telemetry and backend
//! SQLite settings (`custom_names`, `thermostat_offset`, `comfort`/`eco` presets)
//! into typed [`DeviceState`] instances.

use std::collections::HashMap;

use serde_json::Value;

use crate::core::domain_constants::hardware::{
    device_capabilities as caps, device_types,
    fritz_api::{DEFAULT_BATTERY_LEVEL, MILLIWATTS_PER_WATT, SUBUNIT_DEFAULT_SUFFIX},
    special_devices::GROUP_ID_PREFIX,
};
use crate::core::psychrometrics::{calculate_apparent_temperature, round_decimals};
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::models::device::{
    BasicFields, DeviceKind, DeviceState, SensorFields, SwitchFields, ThermostatFields,
};
use crate::services::fritz::RawAhaDevice;

/// Floating-point threshold (`0.01 °C`) for detecting meaningful setpoint/offset deltas.
pub const TEMP_CHANGE_THRESHOLD: f64 = 0.01;

// =============================================================================
/// Software-group membership metadata attached to grouped devices (`Single Authority`).
#[derive(Debug, Clone)]
pub struct GroupAuthorityInfo {
    /// Prefixed software-group ID (`"group-1"`).
    pub prefixed_id: String,
    /// Human-readable group name (`"Wohnzimmer"`).
    pub name: String,
}

// =============================================================================
/// Builds `normalized_ain -> GroupAuthorityInfo` (including HAN-FUN `-1` aliases).
#[must_use]
pub fn build_software_group_ain_map(groups: &[Value]) -> HashMap<String, GroupAuthorityInfo> {
    let mut map = HashMap::new();
    for group in groups {
        let Some(gid) = group.get("id").and_then(Value::as_str) else {
            continue;
        };
        let gname = group
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Unknown")
            .to_owned();
        let info = GroupAuthorityInfo {
            prefixed_id: format!("{GROUP_ID_PREFIX}{gid}"),
            name: gname,
        };
        let Some(members) = group.get("members").and_then(Value::as_array) else {
            continue;
        };
        for raw_member in members.iter().filter_map(Value::as_str) {
            let norm = normalize_ain(raw_member);
            let alt = hanfun_alt_ain(&norm);
            map.insert(norm, info.clone());
            if let Some(alt_key) = alt {
                map.entry(alt_key).or_insert_with(|| info.clone());
            }
        }
    }
    map
}

// =============================================================================
/// Applies `Single Authority` group lock fields onto `state`.
pub fn apply_software_group_authority(state: &mut DeviceState, info: Option<&GroupAuthorityInfo>) {
    if let Some(grp) = info {
        state.group_id = Some(grp.prefixed_id.clone());
        state.automation_locked = true;
        state.managed_by = Some(grp.name.clone());
    } else {
        state.group_id = None;
        state.automation_locked = false;
        state.managed_by = None;
    }
}

// =============================================================================
/// Resolves a backend custom display name for `ain`, falling back to HAN-FUN base/sub-unit AIN.
#[must_use]
pub fn resolve_display_name(
    ain: &str,
    norm_ain: &str,
    default_name: &str,
    custom_names: &HashMap<String, String>,
) -> String {
    if let Some(exact) = custom_names.get(ain) {
        return exact.clone();
    }
    if let Some(by_norm) = custom_names.get(norm_ain) {
        return by_norm.clone();
    }
    let alt = hanfun_alt_ain(norm_ain);
    for (custom_ain, cname) in custom_names {
        let c_norm = normalize_ain(custom_ain);
        if c_norm == norm_ain || alt.as_deref() == Some(c_norm.as_str()) {
            return cname.clone();
        }
    }
    default_name.to_owned()
}

// =============================================================================
/// Constructs a typed [`DeviceState`] from [`RawAhaDevice`] and backend SQLite settings.
#[must_use]
pub fn build_device_state(
    raw: &RawAhaDevice,
    display_name: String,
    group_ain: Option<String>,
    db: &Database,
) -> DeviceState {
    let humidity = raw.humidity;
    let capabilities = raw.resolve_capabilities(humidity);
    let has_cap = |token: &str| capabilities.iter().any(|c| c == token);

    let mut state = if has_cap(caps::THERMOSTAT) {
        create_thermostat_state(raw, display_name, group_ain.is_some(), humidity, db)
    } else if has_cap(caps::SWITCH) || has_cap(caps::POWER_METER) {
        create_switch_state(raw, display_name, &capabilities)
    } else if has_cap(caps::TEMPERATURE_SENSOR)
        || has_cap(caps::HUMIDITY_SENSOR)
        || has_cap(caps::BUTTON)
        || has_cap(caps::ALARM_SENSOR)
    {
        create_sensor_state(raw, display_name, humidity)
    } else {
        DeviceState::new(
            raw.ain.clone(),
            display_name,
            device_types::GENERIC,
            DeviceKind::Basic(BasicFields {}),
        )
    };
    state.present = raw.present;
    state.product_name = raw.product_name.clone();
    state.capabilities = capabilities;
    state.group_ain = group_ain;
    state.humidity = humidity;
    state
}

// =============================================================================
/// Builds a thermostat [`DeviceState`] separating `temperature_hardware` and `temperature_offset`.
fn create_thermostat_state(
    raw: &RawAhaDevice,
    name: String,
    is_grouped: bool,
    humidity: Option<f64>,
    db: &Database,
) -> DeviceState {
    let raw_temp = raw.temperature.or(raw.hkr_temp).unwrap_or(0.0);
    let fritz_off = raw.temperature_offset.unwrap_or(0.0);
    let cow_off = db.get_thermostat_offset(&raw.ain);
    let hw_temp = if raw_temp > 0.0 {
        round_decimals(raw_temp - fritz_off, 1)
    } else {
        0.0
    };
    let eff_off = if !is_grouped && fritz_off.abs() < TEMP_CHANGE_THRESHOLD {
        cow_off
    } else {
        0.0
    };
    let cur_temp = if hw_temp > 0.0 {
        round_decimals(hw_temp + eff_off, 1)
    } else {
        0.0
    };
    let app_temp = humidity
        .filter(|&rh| cur_temp > 0.0 && rh > 0.0)
        .map(|rh| calculate_apparent_temperature(cur_temp, rh));
    let (comfort, eco) = db.get_thermostat_presets(&raw.ain);
    let raw_target = raw.target_temperature.unwrap_or(0.0);

    DeviceState::new_thermostat(
        raw.ain.clone(),
        name,
        ThermostatFields {
            temperature_current: cur_temp,
            temperature_hardware: Some(hw_temp),
            temperature_target: raw_target,
            hardware_target_temperature: Some(raw_target),
            comfort_temperature: Some(comfort),
            eco_temperature: Some(eco),
            battery_level: raw.battery_level.unwrap_or(DEFAULT_BATTERY_LEVEL),
            battery_low: raw.battery_low,
            window_open: raw.window_open,
            boost_active: raw.boost_active,
            error_code: raw.error_code,
            apparent_temperature: app_temp,
            temperature_offset: cow_off,
            fritz_temperature_offset: fritz_off,
            ..ThermostatFields::default()
        },
    )
}

// =============================================================================
/// Builds a smart switch or optical grid meter [`DeviceState`].
fn create_switch_state(raw: &RawAhaDevice, name: String, capabilities: &[String]) -> DeviceState {
    let has_relay = capabilities.iter().any(|c| c == caps::SWITCH);
    let is_on = if has_relay {
        raw.switch_state.unwrap_or(false)
    } else {
        raw.present
    };
    let power_w = raw
        .power_mw
        .map_or(0.0, |mw| mw as f64 / MILLIWATTS_PER_WATT);
    DeviceState::new_switch(
        raw.ain.clone(),
        name,
        SwitchFields {
            state: is_on,
            power_current: power_w,
            energy_total: raw.energy_wh.map_or(0.0, |wh| wh as f64),
            voltage: raw.voltage.unwrap_or(0.0),
            temperature_current: raw.temperature.unwrap_or(0.0),
            battery_level: raw.battery_level,
            battery_low: raw.battery_low,
        },
    )
}

// =============================================================================
/// Builds a climate / button / contact sensor [`DeviceState`] (`FRITZ!Smart Control 440`).
fn create_sensor_state(raw: &RawAhaDevice, name: String, humidity: Option<f64>) -> DeviceState {
    let app_temp = match (raw.temperature, humidity) {
        (Some(t), Some(rh)) if t > 0.0 && rh > 0.0 => Some(calculate_apparent_temperature(t, rh)),
        _ => None,
    };
    DeviceState::new_sensor(
        raw.ain.clone(),
        name,
        SensorFields {
            temperature_current: raw.temperature,
            temperature_offset: raw.temperature_offset,
            apparent_temperature: app_temp,
            battery_level: raw.battery_level,
            battery_low: raw.battery_low,
            last_pressed: raw.last_pressed,
        },
    )
}

// =============================================================================
/// Computes the paired HAN-FUN base or `-1` sub-unit normalized AIN, if numeric.
fn hanfun_alt_ain(norm_ain: &str) -> Option<String> {
    if let Some((base, suffix)) = norm_ain.rsplit_once('-') {
        if suffix.chars().all(|c| c.is_ascii_digit()) && base.chars().all(|c| c.is_ascii_digit()) {
            return Some(base.to_owned());
        }
        None
    } else if !norm_ain.is_empty() && norm_ain.chars().all(|c| c.is_ascii_digit()) {
        Some(format!("{norm_ain}{SUBUNIT_DEFAULT_SUFFIX}"))
    } else {
        None
    }
}
