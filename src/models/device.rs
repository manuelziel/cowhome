//! Strongly typed Digital Twin device models (`Thermostat`, `Switch`, `Sensor`, `Router`).
//!
//! Mirrors `02_linux/cowhome/src/models/state.py` 1:1 on the wire while providing
//! compile-time type safety and excluding transient worker hold flags from JSON output.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core::domain_constants::hardware::{
    device_capabilities, device_types,
    fritz_api::{thermostat_error_description, DEFAULT_BATTERY_LEVEL, DEFAULT_THERMOSTAT_OFFSET},
    special_devices::{ROUTER_AIN, ROUTER_NAME},
};
use crate::core::psychrometrics::DevicePsychrometrics;
use crate::core::time_util::now_local_iso8601;

// =============================================================================
///Unified Digital Twin entity representing any Smart Home device or `ROUTER_01`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    /// FritzBox Actor Identification Number (or `"ROUTER_01"`).
    pub ain: String,
    /// Display name (custom backend name if set, otherwise FritzBox hardware name).
    pub name: String,
    /// Standardized class (`"thermostat"`, `"switch"`, `"sensor"`, `"router"`, `"generic"`).
    pub device_type: String,
    /// True when the device is currently reachable via DECT-ULE / HAN-FUN / TR-064.
    pub present: bool,
    /// Hardware model string reported by the FritzBox (`"FRITZ!DECT 301"`, etc.).
    pub product_name: Option<String>,
    /// Standardized capability tokens (`"thermostat"`, `"power_meter"`, etc.).
    pub capabilities: Vec<String>,
    /// Optional raw hardware group AIN if reported.
    pub group_ain: Option<String>,
    /// Assigned backend software-group ID (`"group-1"`), or `None`.
    pub group_id: Option<String>,
    /// True when individual device automations are locked by an active software group.
    pub automation_locked: bool,
    /// Display name of the controlling software group (`"Wohnzimmer"`), or `None`.
    pub managed_by: Option<String>,
    /// Relative humidity (`%`), native or propagated from a linked room sensor.
    pub humidity: Option<f64>,
    /// Dew point temperature (`°C`).
    pub dew_point_c: Option<f64>,
    /// Absolute indoor humidity (`g/m³`).
    pub abs_humidity_gm3: Option<f64>,
    /// Critical thermal-bridge surface relative humidity (`%`, `DIN 4108-2`).
    pub wall_surface_humidity_pct: Option<f64>,
    /// Projected relative humidity (`%`) at baseline target temperature.
    pub projected_base_humidity_pct: Option<f64>,
    /// True when `HumidityControl` thermal shielding is actively boosting target temperature.
    pub humidity_guard_active: bool,
    /// Outdoor-aware ventilation recommendation (`"none"`, `"ventilate_now"`, etc.).
    pub ventilation_advice: Option<String>,
    /// Indoor-minus-outdoor absolute humidity delta (`g/m³`).
    pub humidity_delta_outdoor_gm3: Option<f64>,
    /// `ISO 8601` timestamp of the most recent state update.
    pub last_updated: String,
    /// Device-class-specific telemetry flattened directly into the JSON object.
    #[serde(flatten)]
    pub kind: DeviceKind,
}

// =============================================================================
/// Class-specific fields flattened into [`DeviceState`] during JSON serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeviceKind {
    /// Radiator thermostat telemetry and calibration state (`FRITZ!DECT 301/302`).
    Thermostat(Box<ThermostatFields>),
    /// Smart switch or optical energy meter telemetry (`FRITZ!Smart Energy 200/210/250`).
    Switch(SwitchFields),
    /// Dedicated climate sensor / wall switch telemetry (`FRITZ!Smart Control 440`).
    Sensor(SensorFields),
    /// Router (`ROUTER_01`) or generic device without extra subclass fields.
    Basic(BasicFields),
}

// =============================================================================
/// Empty struct backing `DeviceKind::Basic` so `#[serde(flatten)]` emits zero extra keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasicFields {}

// =============================================================================
/// Radiator thermostat telemetry, calibration offsets, and `Pre-Heat` status fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermostatFields {
    /// Authoritative room temperature (`°C`, from linked sensor or calibrated valve).
    pub temperature_current: f64,
    /// Raw uncalibrated temperature (`°C`) measured directly at the radiator valve.
    pub temperature_hardware: Option<f64>,
    /// User- or schedule-requested room target temperature (`°C`).
    pub temperature_target: f64,
    /// Compensated target temperature (`°C`) dispatched to `sethkrtsoll`.
    pub hardware_target_temperature: Option<f64>,
    /// Backend-managed comfort preset temperature (`°C`).
    pub comfort_temperature: Option<f64>,
    /// Backend-managed eco setback preset temperature (`°C`).
    pub eco_temperature: Option<f64>,
    /// Battery charge percentage (`0`..=`100`).
    pub battery_level: u8,
    /// Hardware low-battery warning flag.
    pub battery_low: bool,
    /// Open-window valve pause state.
    pub window_open: bool,
    /// Rapid-heating boost mode state.
    pub boost_active: bool,
    /// Numeric AVM valve error code (`0`..=`6`).
    pub error_code: u8,
    /// Human-readable AVM valve error description synchronized with `error_code`.
    pub error_description: String,
    /// Calculated indoor apparent temperature (`°C`).
    pub apparent_temperature: Option<f64>,
    /// Linked external room climate sensor AIN (`FRITZ!Smart Control 440`), if bound.
    pub reference_sensor_ain: Option<String>,
    /// CowHome standalone calibration offset (`-5.0`..=`+5.0 °C` in `0.5 °C` steps).
    pub temperature_offset: f64,
    /// Internal FritzBox hardware offset (`°C`, expected `0.0 °C`).
    pub fritz_temperature_offset: f64,
    /// True when `AdaptiveStart` (`Pre-Heat`) is actively warming the room early.
    pub preheat_active: bool,
    /// Upcoming comfort block target temperature (`°C`) during active `Pre-Heat`.
    pub preheat_target_temp: Option<f64>,
    /// Local `"HH:MM"` start time of the upcoming comfort block during `Pre-Heat`.
    pub preheat_target_time: Option<String>,
    /// Authoritative arbitration pause reason (`"vacation_mode"`, `"window_open"`, etc.).
    pub preheat_paused_reason: Option<String>,
    /// Transient UNIX epoch (`s`) until which software target overrides DECT-ULE poll lag.
    #[serde(skip)]
    pub target_hold_until: f64,
    /// Transient cycle flag indicating active dynamic room-sensor offset control.
    #[serde(skip)]
    pub offset_control_active: bool,
}

impl Default for ThermostatFields {
    /// Returns zero-initialized radiator thermostat telemetry with `100 %` battery default.
    fn default() -> Self {
        Self {
            temperature_current: 0.0,
            temperature_hardware: None,
            temperature_target: 0.0,
            hardware_target_temperature: None,
            comfort_temperature: None,
            eco_temperature: None,
            battery_level: DEFAULT_BATTERY_LEVEL,
            battery_low: false,
            window_open: false,
            boost_active: false,
            error_code: 0,
            error_description: thermostat_error_description(0).to_owned(),
            apparent_temperature: None,
            reference_sensor_ain: None,
            temperature_offset: DEFAULT_THERMOSTAT_OFFSET,
            fritz_temperature_offset: DEFAULT_THERMOSTAT_OFFSET,
            preheat_active: false,
            preheat_target_temp: None,
            preheat_target_time: None,
            preheat_paused_reason: None,
            target_hold_until: 0.0,
            offset_control_active: false,
        }
    }
}

// =============================================================================
/// Smart switch/outlet or optical grid meter (`FRITZ!Smart Energy 200/210/250`) fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwitchFields {
    /// Relay ON/OFF state (or online state for relay-less optical grid meters).
    pub state: bool,
    /// Instantaneous active power (`W`, negative during solar feed-in).
    pub power_current: f64,
    /// Cumulative energy counter (`Wh`).
    pub energy_total: f64,
    /// Mains voltage (`V`).
    pub voltage: f64,
    /// Ambient device temperature (`°C`).
    pub temperature_current: f64,
    /// Optional battery charge (`0`..=`100 %`) for battery-powered meters (`Smart Energy 250`).
    pub battery_level: Option<u8>,
    /// Low-battery warning flag.
    pub battery_low: bool,
}

// =============================================================================
/// Dedicated climate sensor or wall button (`FRITZ!Smart Control 440`) fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SensorFields {
    /// Measured room temperature (`°C`).
    pub temperature_current: Option<f64>,
    /// Sensor temperature offset (`°C`), if reported.
    pub temperature_offset: Option<f64>,
    /// Calculated indoor apparent temperature (`°C`).
    pub apparent_temperature: Option<f64>,
    /// Battery charge percentage (`0`..=`100`).
    pub battery_level: Option<u8>,
    /// Low-battery warning flag.
    pub battery_low: bool,
    /// UNIX epoch seconds of the most recent physical button press.
    pub last_pressed: Option<i64>,
}

impl DeviceState {
    /// Constructs a base [`DeviceState`] with shared metadata and `kind`.
    #[must_use]
    pub fn new(
        ain: impl Into<String>,
        name: impl Into<String>,
        device_type: &str,
        kind: DeviceKind,
    ) -> Self {
        Self {
            ain: ain.into(),
            name: name.into(),
            device_type: device_type.to_owned(),
            present: false,
            product_name: None,
            capabilities: Vec::new(),
            group_ain: None,
            group_id: None,
            automation_locked: false,
            managed_by: None,
            humidity: None,
            dew_point_c: None,
            abs_humidity_gm3: None,
            wall_surface_humidity_pct: None,
            projected_base_humidity_pct: None,
            humidity_guard_active: false,
            ventilation_advice: None,
            humidity_delta_outdoor_gm3: None,
            last_updated: now_local_iso8601(),
            kind,
        }
    }

    /// Constructs a thermostat [`DeviceState`] with synchronized `error_description`.
    #[must_use]
    pub fn new_thermostat(
        ain: impl Into<String>,
        name: impl Into<String>,
        mut fields: ThermostatFields,
    ) -> Self {
        fields.error_description = thermostat_error_description(fields.error_code).to_owned();
        Self::new(
            ain,
            name,
            device_types::THERMOSTAT,
            DeviceKind::Thermostat(Box::new(fields)),
        )
    }

    /// Constructs a smart switch or power meter [`DeviceState`].
    #[must_use]
    pub fn new_switch(
        ain: impl Into<String>,
        name: impl Into<String>,
        fields: SwitchFields,
    ) -> Self {
        Self::new(ain, name, device_types::SWITCH, DeviceKind::Switch(fields))
    }

    /// Constructs a climate/button sensor [`DeviceState`].
    #[must_use]
    pub fn new_sensor(
        ain: impl Into<String>,
        name: impl Into<String>,
        fields: SensorFields,
    ) -> Self {
        Self::new(ain, name, device_types::SENSOR, DeviceKind::Sensor(fields))
    }

    /// Constructs the virtual FritzBox gateway entity (`ROUTER_01`).
    #[must_use]
    pub fn new_router(present: bool, model_name: Option<String>) -> Self {
        let mut dev = Self::new(
            ROUTER_AIN,
            ROUTER_NAME,
            device_types::ROUTER,
            DeviceKind::Basic(BasicFields {}),
        );
        dev.present = present;
        dev.product_name = model_name;
        dev.capabilities = vec![device_capabilities::ROUTER.to_owned()];
        dev
    }

    /// Returns a shared reference to [`ThermostatFields`] if this device is a thermostat.
    #[must_use]
    pub fn as_thermostat(&self) -> Option<&ThermostatFields> {
        match &self.kind {
            DeviceKind::Thermostat(t) => Some(t),
            _ => None,
        }
    }

    /// Returns a mutable reference to [`ThermostatFields`] if this device is a thermostat.
    pub fn as_thermostat_mut(&mut self) -> Option<&mut ThermostatFields> {
        match &mut self.kind {
            DeviceKind::Thermostat(t) => Some(t),
            _ => None,
        }
    }

    /// Returns a shared reference to [`SwitchFields`] if this device is a switch/meter.
    #[must_use]
    pub fn as_switch(&self) -> Option<&SwitchFields> {
        match &self.kind {
            DeviceKind::Switch(s) => Some(s),
            _ => None,
        }
    }

    /// Returns a mutable reference to [`SwitchFields`] if this device is a switch/meter.
    pub fn as_switch_mut(&mut self) -> Option<&mut SwitchFields> {
        match &mut self.kind {
            DeviceKind::Switch(s) => Some(s),
            _ => None,
        }
    }

    /// Returns a shared reference to [`SensorFields`] if this device is a sensor.
    #[must_use]
    pub fn as_sensor(&self) -> Option<&SensorFields> {
        match &self.kind {
            DeviceKind::Sensor(s) => Some(s),
            _ => None,
        }
    }

    /// Applies a computed [`DevicePsychrometrics`] bundle onto this device.
    pub fn apply_psychrometrics(&mut self, psych: DevicePsychrometrics) {
        self.dew_point_c = psych.dew_point_c;
        self.abs_humidity_gm3 = psych.abs_humidity_gm3;
        self.wall_surface_humidity_pct = psych.wall_surface_humidity_pct;
        self.projected_base_humidity_pct = psych.projected_base_humidity_pct;
        self.humidity_guard_active = psych.humidity_guard_active;
        self.ventilation_advice = psych.ventilation_advice;
        self.humidity_delta_outdoor_gm3 = psych.humidity_delta_outdoor_gm3;
    }

    /// Serializes the device to a JSON [`Value`], synchronizing `error_description`.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        let mut copy = self.clone();
        if let DeviceKind::Thermostat(ref mut t) = copy.kind {
            t.error_description = thermostat_error_description(t.error_code).to_owned();
        }
        serde_json::to_value(copy).unwrap_or_else(|_| json!({}))
    }
}
