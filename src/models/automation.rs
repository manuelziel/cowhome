//! Automation rule configuration models, scope resolution, and client alias mapping.
//!
//! Defines strongly typed structs for all 7 parameterized Smart Features stored in
//! `automation_config`, matching `02_linux/cowhome/src/models/automation.py` 1:1.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::domain_constants::{
    automation::{
        automation_defaults::{
            FROST_PROTECTION_TEMP, HUMIDITY_MAX_TEMP_BOOST_C, HUMIDITY_MAX_TEMP_BOOST_LIMIT_C,
            HUMIDITY_MAX_THRESHOLD, HUMIDITY_MIN_TEMP_BOOST_C, HUMIDITY_MIN_THRESHOLD,
            HUMIDITY_THRESHOLD, MAX_DYNAMIC_TEMP_OFFSET, OUTDOOR_TEMP_THRESHOLD,
            PRESENCE_AWAY_TEMP_OFFSET, PRESENCE_TIMEOUT_MINUTES, SCHEDULE_DEFAULT_COMFORT_TEMP,
            SCHEDULE_DEFAULT_ECO_TEMP, SUMMER_PAUSE_END_DATE, SUMMER_PAUSE_START_DATE,
            VACATION_PRE_HEAT_HOURS,
        },
        automation_identifiers::{
            ADAPTIVE_START, ALL_FEATURES, APPARENT_TEMPERATURE, HEATING_SCHEDULE, HUMIDITY_CONTROL,
            POWER_SCHEDULE_ALIAS, PRESENCE_PROFILING, SCOPE_DEVICE, SCOPE_GLOBAL, SCOPE_GROUP,
            VACATION_MODE, WEATHER_SMART_HEATING,
        },
    },
    hardware::fritz_api::HALF_DEGREE_FACTOR,
};
use crate::core::error::{AppError, AppResult};
use crate::core::heating_forecast::canonical_weekday_key;
use crate::models::group::parse_group_id;

/// Legacy client field aliases `(feature_filter, alias_key, canonical_key)` mapped before deserializing.
const CONFIG_KEY_ALIASES: &[(&str, &str, &str)] = &[
    (
        WEATHER_SMART_HEATING,
        "threshold_temp",
        "outdoor_temp_threshold",
    ),
    (
        HUMIDITY_CONTROL,
        "max_humidity_threshold",
        "humidity_threshold",
    ),
    (HUMIDITY_CONTROL, "max_boost_c", "max_temp_boost_c"),
    (HUMIDITY_CONTROL, "reference_sensor_ain", "sensor_ain"),
    (VACATION_MODE, "target_temp", "frost_protection_temp"),
    (APPARENT_TEMPERATURE, "reference_sensor_ain", "sensor_ain"),
    (ADAPTIVE_START, "target_temperature", "target_temp"),
    (ADAPTIVE_START, "target_time", "target_time_cron"),
    (PRESENCE_PROFILING, "eco_offset", "away_temp_offset"),
    (HEATING_SCHEDULE, "early_start_enabled", "preheat_enabled"),
    (HEATING_SCHEDULE, "early_start", "preheat_enabled"),
    (
        HEATING_SCHEDULE,
        "adaptive_start_enabled",
        "preheat_enabled",
    ),
];

// =============================================================================
/// Resolves a feature name or alias (`"PowerSchedule"`, `"heating_schedule"`) to its canonical key.
#[must_use]
pub fn normalize_feature_name(raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    for &feat in ALL_FEATURES {
        if trimmed.eq_ignore_ascii_case(feat) {
            return Some(feat);
        }
    }
    match trimmed.to_ascii_lowercase().as_str() {
        s if s == POWER_SCHEDULE_ALIAS.to_ascii_lowercase()
            || matches!(s, "power_schedule" | "heating_schedule") =>
        {
            Some(HEATING_SCHEDULE)
        }
        "weather_smart_heating" => Some(WEATHER_SMART_HEATING),
        "humidity_control" => Some(HUMIDITY_CONTROL),
        "vacation_mode" => Some(VACATION_MODE),
        "apparent_temperature" => Some(APPARENT_TEMPERATURE),
        "adaptive_start" => Some(ADAPTIVE_START),
        "presence_profiling" => Some(PRESENCE_PROFILING),
        _ => None,
    }
}

// =============================================================================
/// Validated automation scope (`global`, `group`, or `device`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AutomationScope {
    /// Scope level (`"global"`, `"group"`, or `"device"`).
    pub scope_type: String,
    /// Scope identifier (`"global"`, `"group-1"`, or device AIN).
    pub scope_id: String,
}

impl Default for AutomationScope {
    /// Returns the house-wide `"global"` automation scope.
    fn default() -> Self {
        Self {
            scope_type: SCOPE_GLOBAL.to_owned(),
            scope_id: SCOPE_GLOBAL.to_owned(),
        }
    }
}

impl AutomationScope {
    /// Parses and validates `scope_type` and `scope_id` query/payload parameters.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `scope_type` is unknown or `group`/`device` omits `scope_id`.
    pub fn parse(raw_type: Option<&str>, raw_id: Option<&str>) -> AppResult<Self> {
        let stype = raw_type
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(SCOPE_GLOBAL)
            .to_ascii_lowercase();
        match stype.as_str() {
            SCOPE_GLOBAL => Ok(Self::default()),
            SCOPE_GROUP => {
                let raw = raw_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        AppError::Validation("scope_id is required when scope='group'".into())
                    })?;
                let gid = parse_group_id(raw).ok_or_else(|| {
                    AppError::Validation(format!("Invalid group scope_id '{raw}'"))
                })?;
                Ok(Self {
                    scope_type: SCOPE_GROUP.to_owned(),
                    scope_id: gid.to_string(),
                })
            }
            SCOPE_DEVICE => {
                let id = raw_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(SCOPE_GLOBAL))
                    .ok_or_else(|| {
                        AppError::Validation("scope_id is required when scope='device'".into())
                    })?;
                Ok(Self {
                    scope_type: SCOPE_DEVICE.to_owned(),
                    scope_id: id.to_owned(),
                })
            }
            other => Err(AppError::Validation(format!(
                "Invalid automation scope '{other}' (expected global, group, or device)"
            ))),
        }
    }
}

// =============================================================================
/// Stored row from `automation_config` combining activation state and JSON parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRecord {
    /// Canonical feature identifier (`"HeatingSchedule"`, etc.).
    pub feature_name: String,
    /// Scope level (`"global"`, `"group"`, or `"device"`).
    pub scope_type: String,
    /// Scope identifier (`"global"`, `"group-1"`, or AIN).
    pub scope_id: String,
    /// Whether the rule actively controls actuators.
    pub is_active: bool,
    /// Normalized JSON configuration object.
    pub config: Value,
    /// SQLite `updated_at` timestamp string.
    pub updated_at: String,
}

// =============================================================================
/// `WeatherSmartHeating` (`Summer Pause`) rule parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherSmartHeatingConfig {
    /// Outdoor temperature (`°C`) above which heating pauses inside `[start_date, end_date]`.
    pub outdoor_temp_threshold: f64,
    /// Inclusive `"MM-DD"` season start date (`"05-01"`).
    pub start_date: String,
    /// Inclusive `"MM-DD"` season end date (`"09-30"`).
    pub end_date: String,
    /// Controlled thermostat AINs.
    pub affected_devices: Vec<String>,
}

impl Default for WeatherSmartHeatingConfig {
    /// Returns default `Summer Pause` parameters (`18.0 °C`, `05-01`..`09-30`).
    fn default() -> Self {
        Self {
            outdoor_temp_threshold: OUTDOOR_TEMP_THRESHOLD,
            start_date: SUMMER_PAUSE_START_DATE.to_owned(),
            end_date: SUMMER_PAUSE_END_DATE.to_owned(),
            affected_devices: Vec::new(),
        }
    }
}

// =============================================================================
/// `HumidityControl` v2 (`DIN 4108-2` Mold & Humidity Guard) rule parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HumidityControlConfig {
    /// Optional room reference humidity sensor AIN (`""` = auto/native).
    pub sensor_ain: String,
    /// Controlled radiator thermostat AINs.
    pub actor_ain: Vec<String>,
    /// Trigger relative humidity threshold (`40.0`..=`90.0 %`, default `60.0 %`).
    pub humidity_threshold: f64,
    /// Maximum guard boost above `T_base` (`0.0`..=`4.0 °C` in `0.5 °C` steps; `0.0` = alert only).
    pub max_temp_boost_c: f64,
}

impl Default for HumidityControlConfig {
    /// Returns default `DIN 4108-2` `HumidityControl` parameters (`60.0 %`, `+2.0 °C`).
    fn default() -> Self {
        Self {
            sensor_ain: String::new(),
            actor_ain: Vec::new(),
            humidity_threshold: HUMIDITY_THRESHOLD,
            max_temp_boost_c: HUMIDITY_MAX_TEMP_BOOST_C,
        }
    }
}

impl HumidityControlConfig {
    /// Clamps thresholds and quantizes `max_temp_boost_c` to safe `0.5 °C` steps.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.actor_ain = self
            .actor_ain
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        self.humidity_threshold = self
            .humidity_threshold
            .clamp(HUMIDITY_MIN_THRESHOLD, HUMIDITY_MAX_THRESHOLD);
        let q_boost = (self.max_temp_boost_c * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR;
        self.max_temp_boost_c =
            q_boost.clamp(HUMIDITY_MIN_TEMP_BOOST_C, HUMIDITY_MAX_TEMP_BOOST_LIMIT_C);
        self
    }
}

// =============================================================================
/// `VacationMode` (`Frost Guard`) global override parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VacationModeConfig {
    /// Planned return UNIX timestamp (`s`, `0.0` = indefinite).
    pub end_date_timestamp: f64,
    /// Frost-guard target temperature (`°C`, default `8.0 °C`).
    pub frost_protection_temp: f64,
    /// Hours before `end_date_timestamp` to resume normal schedules (`6 h`).
    pub pre_heat_hours: u64,
    /// Exempt device AINs.
    pub ignored_devices: Vec<String>,
    /// Exempt software-group IDs.
    pub ignored_groups: Vec<String>,
}

impl Default for VacationModeConfig {
    /// Returns default `Frost Guard` parameters (`8.0 °C`, `6 h` pre-return wakeup).
    fn default() -> Self {
        Self {
            end_date_timestamp: 0.0,
            frost_protection_temp: FROST_PROTECTION_TEMP,
            pre_heat_hours: VACATION_PRE_HEAT_HOURS,
            ignored_devices: Vec::new(),
            ignored_groups: Vec::new(),
        }
    }
}

// =============================================================================
/// `ApparentTemperature` (`Room Sensor Sync`) master-slave parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApparentTemperatureConfig {
    /// Master room climate sensor AIN (`FRITZ!Smart Control 440`).
    pub sensor_ain: String,
    /// Optional global thermostat-to-sensor mapping.
    pub sensor_mapping: HashMap<String, String>,
    /// Actively compensate `hardware_target_temperature` to track `sensor_ain`.
    pub active_offset_control: bool,
    /// Maximum dynamic valve offset (`±4.0 °C`).
    pub max_offset: f64,
}

impl Default for ApparentTemperatureConfig {
    /// Returns default `Room Sensor Sync` parameters (`active_offset_control = true`, `±4.0 °C`).
    fn default() -> Self {
        Self {
            sensor_ain: String::new(),
            sensor_mapping: HashMap::new(),
            active_offset_control: true,
            max_offset: MAX_DYNAMIC_TEMP_OFFSET,
        }
    }
}

// =============================================================================
/// Single 30-minute or ranged time block inside [`HeatingScheduleConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSlot {
    /// Block start wall-clock time (`"HH:MM"`).
    pub start: String,
    /// Block end wall-clock time (`"HH:MM"`).
    pub end: String,
    /// Optional block target temperature (`°C`) overriding `comfort_temp`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp: Option<f64>,
    /// Optional explicit switch relay state (`true` = ON, `false` = OFF).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<bool>,
}

// =============================================================================
/// `HeatingSchedule` / `PowerSchedule` 7-day weekly schedule parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeatingScheduleConfig {
    /// Setback temperature (`°C`) outside active comfort blocks (`18.0 °C`).
    pub eco_temp: f64,
    /// Default comfort temperature (`°C`) inside active blocks (`22.0 °C`).
    pub comfort_temp: f64,
    /// Enables automatic `DIN EN 12098-1` `Pre-Heat` (`AdaptiveStart`) before blocks.
    pub preheat_enabled: bool,
    /// Controlled thermostat or smart-switch AINs.
    pub affected_devices: Vec<String>,
    /// Weekday schedule map (`"mon"`..`"sun"` -> list of JSON slot objects).
    pub days: HashMap<String, Vec<Value>>,
}

impl Default for HeatingScheduleConfig {
    /// Returns default `HeatingSchedule` parameters (`22.0 °C` comfort, `18.0 °C` eco).
    fn default() -> Self {
        Self {
            eco_temp: SCHEDULE_DEFAULT_ECO_TEMP,
            comfort_temp: SCHEDULE_DEFAULT_COMFORT_TEMP,
            preheat_enabled: false,
            affected_devices: Vec::new(),
            days: HashMap::new(),
        }
    }
}

// =============================================================================
/// `AdaptiveStart` (`Pre-Heat`) rule parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptiveStartConfig {
    /// Primary target thermostat AIN.
    pub target_ain: String,
    /// Controlled thermostat AIN list.
    pub target_ains: Vec<String>,
    /// Fallback target `"HH:MM"` time (`"07:00"`).
    pub target_time_cron: String,
    /// Fallback target comfort temperature (`22.0 °C`).
    pub target_temp: f64,
    /// Optional learned presence matrix.
    pub presence_schedule: Map<String, Value>,
}

impl Default for AdaptiveStartConfig {
    /// Returns default `AdaptiveStart` (`Pre-Heat`) parameters (`07:00`, `22.0 °C`).
    fn default() -> Self {
        Self {
            target_ain: String::new(),
            target_ains: Vec::new(),
            target_time_cron: String::from("07:00"),
            target_temp: SCHEDULE_DEFAULT_COMFORT_TEMP,
            presence_schedule: Map::new(),
        }
    }
}

// =============================================================================
/// `PresenceProfiling` (`Auto-Away` via TR-064 Wi-Fi hosts) rule parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresenceProfilingConfig {
    /// Smartphone MAC addresses monitored on the FritzBox WLAN.
    pub tracked_mac_addresses: Vec<String>,
    /// Controlled thermostat AINs.
    pub affected_devices: Vec<String>,
    /// Setback offset (`°C`, default `-3.0 °C`) applied when all MACs are away.
    pub away_temp_offset: f64,
    /// When `true`, records presence history without altering thermostat targets.
    pub learning_mode_only: bool,
    /// Offline grace period in minutes (`15 min`) before triggering auto-away.
    pub timeout_minutes: u64,
}

impl Default for PresenceProfilingConfig {
    /// Returns default `PresenceProfiling` parameters (`-3.0 °C`, `15 min`, learning mode).
    fn default() -> Self {
        Self {
            tracked_mac_addresses: Vec::new(),
            affected_devices: Vec::new(),
            away_temp_offset: PRESENCE_AWAY_TEMP_OFFSET,
            learning_mode_only: true,
            timeout_minutes: PRESENCE_TIMEOUT_MINUTES,
        }
    }
}

// =============================================================================
/// Normalizes client aliases and weekday keys inside raw `config_json` payloads.
#[must_use]
pub fn normalize_config_payload(feature_name: &str, raw: &Value) -> Value {
    let Some(src_map) = raw.as_object() else {
        return Value::Object(Map::new());
    };
    let mut map: Map<String, Value> = src_map
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for &(feat, alias, canonical) in CONFIG_KEY_ALIASES {
        if feat == feature_name
            && !map.contains_key(canonical)
            && let Some(val) = map.get(alias).cloned()
        {
            map.insert(canonical.to_owned(), val);
        }
    }
    for list_key in ["actor_ain", "affected_devices", "target_ains"] {
        if let Some(s) = map.get(list_key).and_then(Value::as_str).map(str::trim) {
            let arr = if s.is_empty() {
                Vec::new()
            } else {
                vec![Value::String(s.to_owned())]
            };
            map.insert(list_key.to_owned(), Value::Array(arr));
        }
    }
    if let Some(s) = map.get("preheat_enabled").and_then(Value::as_str) {
        let b = matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
        map.insert(String::from("preheat_enabled"), Value::Bool(b));
    }
    if feature_name == HEATING_SCHEDULE {
        normalize_schedule_days_in_place(&mut map);
    }
    Value::Object(map)
}

// =============================================================================
/// Normalizes `"days"` / `"schedule"` maps to canonical `"mon"`..`"sun"` keys.
fn normalize_schedule_days_in_place(map: &mut Map<String, Value>) {
    let raw_days = map
        .get("days")
        .or_else(|| map.get("schedule"))
        .and_then(Value::as_object)
        .cloned();
    let source = raw_days.unwrap_or_else(|| {
        map.iter()
            .filter(|(k, v)| canonical_weekday_key(k).is_some() && v.is_array())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    });
    if !source.is_empty() {
        let mut clean_days = Map::new();
        for (k, v) in source {
            if let Some(canon) = canonical_weekday_key(&k)
                && v.is_array()
            {
                clean_days.insert(canon.to_owned(), v);
            }
        }
        map.insert(String::from("days"), Value::Object(clean_days));
    }
}
