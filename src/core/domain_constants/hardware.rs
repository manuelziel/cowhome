//! Hardware, FritzBox AHA/TR-064 protocol, and Digital Twin capability constants.
//!
//! Defines standardized device types, capabilities, actuator action names, AVM
//! `functionbitmask` bit flags, XML element tags, and HKR valve error descriptions.

// =============================================================================
/// Standardized string identifiers for Smart Home device classes.
pub mod device_types {
    /// Radiator thermostat (`FRITZ!DECT 301` / `FRITZ!Smart Thermo 302`).
    pub const THERMOSTAT: &str = "thermostat";
    /// Virtual backend room group (`group-<id>`).
    pub const THERMOSTAT_GROUP: &str = "thermostat_group";
    /// Smart plug or optical energy meter (`FRITZ!Smart Energy 200/210/250`).
    pub const SWITCH: &str = "switch";
    /// Dedicated room climate sensor / wall button (`FRITZ!Smart Control 440`).
    pub const SENSOR: &str = "sensor";
    /// Virtual outdoor weather entity (`WEATHER_01`).
    pub const WEATHER: &str = "weather";
    /// FritzBox gateway router entity (`ROUTER_01`).
    pub const ROUTER: &str = "router";
    /// Fallback identifier for unclassified AHA devices.
    pub const GENERIC: &str = "generic";
}

// =============================================================================
/// Standardized capability tokens exposed in `GET /api/v1/state`.
pub mod device_capabilities {
    /// Target temperature control (`sethkrtsoll`).
    pub const THERMOSTAT: &str = "thermostat";
    /// Ambient or radiator temperature telemetry.
    pub const TEMPERATURE_SENSOR: &str = "temperature_sensor";
    /// Relative humidity telemetry (`0`..=`100 %`).
    pub const HUMIDITY_SENSOR: &str = "humidity_sensor";
    /// Battery charge level and low-battery warning flag.
    pub const BATTERY: &str = "battery";
    /// Open-window detection state.
    pub const WINDOW_DETECTION: &str = "window_detection";
    /// Relay on/off switching (`setswitchon` / `setswitchoff`).
    pub const SWITCH: &str = "switch";
    /// Active power (`W`), voltage (`V`), and total energy (`Wh`) metering.
    pub const POWER_METER: &str = "power_meter";
    /// Physical push-button event timestamps.
    pub const BUTTON: &str = "button";
    /// Smart LED bulb capability (`FRITZ!DECT 500`).
    pub const LIGHTBULB: &str = "lightbulb";
    /// Hue/saturation and Kelvin color-temperature control.
    pub const COLOR_CONTROL: &str = "color_control";
    /// Percentage level control (`0`..=`100 %`).
    pub const LEVEL_CONTROL: &str = "level_control";
    /// Roller-blind actuator (`open` / `close` / `stop`).
    pub const BLIND: &str = "blind";
    /// Binary contact or alarm sensor (`FRITZ!DECT 350`).
    pub const ALARM_SENSOR: &str = "alarm_sensor";
    /// DECT / Wi-Fi repeater capability.
    pub const REPEATER: &str = "repeater";
    /// FritzBox CPE gateway capability.
    pub const ROUTER: &str = "router";
}

// =============================================================================
/// Valid actuator command identifiers for `POST /api/v1/devices/{id}/action`.
pub mod device_actions {
    /// Turn a smart switch explicitly ON or OFF.
    pub const SET_SWITCH: &str = "set_switch";
    /// Invert the current state of a smart switch.
    pub const TOGGLE_SWITCH: &str = "toggle_switch";
    /// Set target room temperature in `0.5 °C` steps (`0.0` = OFF).
    pub const SET_TEMPERATURE: &str = "set_temperature";
    /// Apply (and optionally update) the backend comfort temperature preset.
    pub const SET_COMFORT: &str = "set_comfort";
    /// Apply (and optionally update) the backend eco temperature preset.
    pub const SET_ECO: &str = "set_eco";
    /// Persist backend comfort and/or eco preset temperatures without switching phase.
    pub const SET_PRESETS: &str = "set_presets";
    /// Drive a roller blind (`open`, `close`, or `stop`).
    pub const SET_BLIND: &str = "set_blind";
    /// Set percentage level (`0`..=`100`) on a dimmable bulb or blind.
    pub const SET_LEVEL: &str = "set_level";
    /// Set HSV hue (`0`..=`359`) and saturation (`0`..=`255`) on a color bulb.
    pub const SET_COLOR: &str = "set_color";
    /// Set white-light color temperature in Kelvin (`2700`..=`6500`).
    pub const SET_COLOR_TEMP: &str = "set_color_temperature";
    /// Accepted shorthand alias for [`SET_COLOR_TEMP`].
    pub const SET_COLOR_TEMP_ALIAS: &str = "set_color_temp";
    /// Assign a backend-managed custom display name in SQLite.
    pub const SET_NAME: &str = "set_name";
    /// Configure standalone thermostat calibration offset (`-5.0`..=`+5.0 °C`).
    pub const SET_OFFSET: &str = "set_offset";
}

// =============================================================================
/// Valid router command identifiers for `POST /api/v1/devices/ROUTER_01/action`.
pub mod router_actions {
    /// Trigger WAN termination and public IP renewal via TR-064/IGD.
    pub const RECONNECT: &str = "reconnect_internet";
    /// Reboot the FritzBox CPE (`DeviceConfig:1#Reboot`).
    pub const REBOOT: &str = "reboot";
    /// Enable or disable main 2.4 GHz / 5 GHz WLAN.
    pub const SET_WLAN: &str = "set_wlan";
    /// Enable or disable guest WLAN.
    pub const SET_GUEST_WLAN: &str = "set_guest_wlan";
    /// Dispatch Wake-on-LAN magic packet via `Hosts:1`.
    pub const WAKE_ON_LAN: &str = "wake_on_lan";
    /// Rename `ROUTER_01` in CowHome SQLite storage.
    pub const SET_NAME: &str = "set_name";
}

// =============================================================================
/// Valid roller-blind movement commands.
pub mod blind_states {
    /// Open roller blind completely.
    pub const OPEN: &str = "open";
    /// Close roller blind completely.
    pub const CLOSE: &str = "close";
    /// Halt current blind travel immediately.
    pub const STOP: &str = "stop";
    /// Supported blind state literals.
    pub const ALL: &[&str] = &[OPEN, CLOSE, STOP];
}

// =============================================================================
/// Virtual entity identifiers and software-group prefixes.
pub mod special_devices {
    /// Prefix prepended to numeric software-group IDs (`group-1`).
    pub const GROUP_ID_PREFIX: &str = "group-";
    /// Virtual AIN representing the FritzBox router in `StateRegistry` & SQLite.
    pub const ROUTER_AIN: &str = "ROUTER_01";
    /// Default display name for `ROUTER_01`.
    pub const ROUTER_NAME: &str = "FritzBox Gateway";
    /// Virtual AIN representing Open-Meteo outdoor climate history in SQLite.
    pub const WEATHER_AIN: &str = "WEATHER_01";
    /// Default display name for `WEATHER_01`.
    pub const WEATHER_NAME: &str = "Outdoor Weather";
}

// =============================================================================
/// AVM AHA-HTTP `functionbitmask` flags (`AHA-HTTP-Interface.pdf`).
pub mod fritz_bitmask {
    /// Bit 0: HAN-FUN base device.
    pub const HAN_FUN_DEVICE: u32 = 1 << 0;
    /// Bit 2: Smart light bulb.
    pub const LIGHTBULB: u32 = 1 << 2;
    /// Bit 4: Alarm / contact sensor.
    pub const ALARM_SENSOR: u32 = 1 << 4;
    /// Bit 5: FritzBox push button.
    pub const BUTTON: u32 = 1 << 5;
    /// Bit 6: Radiator thermostat (`HKR`).
    pub const THERMOSTAT: u32 = 1 << 6;
    /// Bit 7: Energy / power meter.
    pub const POWER_METER: u32 = 1 << 7;
    /// Bit 8: Temperature sensor.
    pub const TEMPERATURE_SENSOR: u32 = 1 << 8;
    /// Bit 9: Switchable outlet relay.
    pub const SWITCH: u32 = 1 << 9;
    /// Bit 10: DECT repeater.
    pub const REPEATER: u32 = 1 << 10;
    /// Bit 11: Built-in microphone.
    pub const MICROPHONE: u32 = 1 << 11;
    /// Bit 13: HAN-FUN / ETSI sub-unit.
    pub const HAN_FUN_UNIT: u32 = 1 << 13;
    /// Bit 15: Simple on/off actuator.
    pub const SIMPLE_ON_OFF: u32 = 1 << 15;
    /// Bit 16: Percentage level control.
    pub const LEVEL_CONTROL: u32 = 1 << 16;
    /// Bit 17: Color / Kelvin light control.
    pub const COLOR_CONTROL: u32 = 1 << 17;
    /// Bit 18: Roller blind control.
    pub const BLIND: u32 = 1 << 18;
    /// Bit 20: Relative humidity sensor.
    pub const HUMIDITY_SENSOR: u32 = 1 << 20;
}

// =============================================================================
/// FritzBox AHA & TR-064 protocol conversion factors, limits, and error codes.
pub mod fritz_api {
    /// Minimum valid radiator target temperature in `°C`.
    pub const MIN_THERMOSTAT_TEMP: f64 = 8.0;
    /// Maximum valid radiator target temperature in `°C`.
    pub const MAX_THERMOSTAT_TEMP: f64 = 28.0;
    /// Minimum standalone calibration offset in `°C`.
    pub const MIN_THERMOSTAT_OFFSET: f64 = -5.0;
    /// Maximum standalone calibration offset in `°C`.
    pub const MAX_THERMOSTAT_OFFSET: f64 = 5.0;
    /// Default calibration offset in `°C`.
    pub const DEFAULT_THERMOSTAT_OFFSET: f64 = 0.0;
    /// Quantization step for target temperatures and calibration offsets (`0.5 °C`).
    pub const OFFSET_STEP: f64 = 0.5;
    /// Prefix for per-thermostat offsets stored in `system_settings`.
    pub const OFFSET_SETTING_KEY_PREFIX: &str = "thermostat_offset:";
    /// Battery charge percentage below which `battery_low` alerts trigger.
    pub const LOW_BATTERY_THRESHOLD: u8 = 20;
    /// Fallback battery percentage when hardware omits `<battery>`.
    pub const DEFAULT_BATTERY_LEVEL: u8 = 100;
    /// Conversion factor from milliwatts (`mW`) to watts (`W`).
    pub const MILLIWATTS_PER_WATT: f64 = 1000.0;
    /// Conversion factor from millivolts (`mV`) to volts (`V`).
    pub const MILLIVOLTS_PER_VOLT: f64 = 1000.0;
    /// Conversion factor from `0.1 °C` integer telemetry to `°C`.
    pub const DECICELSIUS_FACTOR: f64 = 10.0;
    /// Conversion factor from `0.5 °C` HKR steps (`16`..=`56`) to `°C`.
    pub const HALF_DEGREE_FACTOR: f64 = 2.0;
    /// Raw AHA `sethkrtsoll` / `<tsoll>` value encoding valve OFF / frost guard.
    pub const TSOLL_OFF_RAW: u16 = 253;
    /// Raw AHA `sethkrtsoll` / `<tsoll>` value encoding valve permanently ON (`BOOST`).
    pub const TSOLL_ON_RAW: u16 = 254;
    /// Primary 2.4 GHz TR-064 `WLANConfiguration` service index.
    pub const MAIN_WLAN_INDEX: u8 = 1;
    /// Secondary 5 GHz TR-064 `WLANConfiguration` service index.
    pub const SECONDARY_WLAN_INDEX: u8 = 2;
    /// Guest Wi-Fi TR-064 `WLANConfiguration` service index on dual-band boxes.
    pub const GUEST_WLAN_INDEX: u8 = 3;
    /// Prefix identifying FritzBox-internal hardware groups (`grp...`), which CowHome ignores.
    pub const GROUP_AIN_PREFIX: &str = "grp";
    /// Default ETSI HAN-FUN sub-unit suffix (`-1`).
    pub const SUBUNIT_DEFAULT_SUFFIX: &str = "-1";
    /// Minimum dimmable/blind percentage level (`0 %`).
    pub const MIN_LEVEL_PERCENT: u8 = 0;
    /// Maximum dimmable/blind percentage level (`100 %`).
    pub const MAX_LEVEL_PERCENT: u8 = 100;
    /// Minimum HSV color hue (`0°`).
    pub const MIN_COLOR_HUE: u16 = 0;
    /// Maximum HSV color hue (`359°`).
    pub const MAX_COLOR_HUE: u16 = 359;
    /// Minimum HSV color saturation (`0`).
    pub const MIN_COLOR_SATURATION: u8 = 0;
    /// Maximum HSV color saturation (`255`).
    pub const MAX_COLOR_SATURATION: u8 = 255;
    /// Warmest supported white-light color temperature in Kelvin (`2700 K`).
    pub const MIN_COLOR_TEMP_KELVIN: u16 = 2700;
    /// Coolest supported white-light color temperature in Kelvin (`6500 K`).
    pub const MAX_COLOR_TEMP_KELVIN: u16 = 6500;
    /// FritzBox AHA HTTP endpoint path.
    pub const AHA_ENDPOINT_PATH: &str = "/webservices/homeautoswitch.lua";
    /// FritzBox AHA v1 login endpoint path.
    pub const AHA_LOGIN_PATH: &str = "/login_sid.lua";
    /// FritzBox AHA v2 PBKDF2 challenge endpoint path with version query.
    pub const AHA_LOGIN_V2_PATH: &str = "/login_sid.lua?version=2";
    /// Prefix identifying a v2 PBKDF2 challenge (`"2$..."`).
    pub const AHA_CHALLENGE_V2_PREFIX: &str = "2$";
    /// AHA command returning the complete XML device telemetry tree.
    pub const CMD_GET_DEVICE_LIST: &str = "getdevicelistinfos";
    /// AHA command turning a switch relay ON.
    pub const CMD_SET_SWITCH_ON: &str = "setswitchon";
    /// AHA command turning a switch relay OFF.
    pub const CMD_SET_SWITCH_OFF: &str = "setswitchoff";
    /// AHA command toggling a switch relay.
    pub const CMD_SET_SWITCH_TOGGLE: &str = "setswitchtoggle";
    /// AHA command setting radiator HKR target temperature (`sethkrtsoll`).
    pub const CMD_SET_HKR_TSOLL: &str = "sethkrtsoll";
    /// AHA command driving a roller blind (`open` / `close` / `stop`).
    pub const CMD_SET_BLIND: &str = "setblind";
    /// AHA command setting dimmable bulb or blind percentage (`0`..=`100`).
    pub const CMD_SET_LEVEL_PERCENTAGE: &str = "setlevelpercentage";
    /// AHA command setting unmapped HSV color on a `FRITZ!DECT 500`.
    pub const CMD_SET_UNMAPPED_COLOR: &str = "setunmappedcolor";
    /// Fallback AHA command setting mapped HSV color.
    pub const CMD_SET_COLOR: &str = "setcolor";
    /// AHA command setting white-light Kelvin color temperature.
    pub const CMD_SET_COLOR_TEMP: &str = "setcolortemperature";
    /// TR-064 WAN connection service candidates probed in priority order.
    pub const WAN_SERVICES: &[&str] = &[
        "WANIPConnection:1",
        "WANPPPConnection:1",
        "WANIPConn:1",
        "WANPPPConn:1",
    ];

    /// Maps an AVM HKR valve `errorcode` (`0`..=`6`) to its human-readable description.
    #[must_use]
    pub const fn thermostat_error_description(code: u8) -> &'static str {
        match code {
            0 => "OK (No error)",
            1 => "Valve adaptation failed (Device mounted on radiator?)",
            2 => "Valve stroke too short or battery power too weak",
            3 => "No valve movement possible (Valve pin stuck?)",
            4 => "Preparing valve adaptation",
            5 => "Installation mode active (Ready for mounting)",
            6 => "Adapting to radiator valve stroke",
            _ => "Unknown hardware valve error",
        }
    }
}

// =============================================================================
/// XML element and attribute names returned by FritzBox `getdevicelistinfos` (`AHA`).
pub mod fritz_xml_tags {
    /// Physical device element (`<device>`).
    pub const DEVICE: &str = "device";
    /// Unique AIN attribute (`identifier`).
    pub const IDENTIFIER: &str = "identifier";
    /// Internal numeric device ID attribute (`id`).
    pub const ID: &str = "id";
    /// Capability bitmask attribute (`functionbitmask`).
    pub const FUNCTION_BITMASK: &str = "functionbitmask";
    /// Hardware model attribute (`productname`).
    pub const PRODUCT_NAME: &str = "productname";
    /// Firmware version attribute (`fwversion`).
    pub const FW_VERSION: &str = "fwversion";
    /// Online flag element (`<present>`).
    pub const PRESENT: &str = "present";
    /// Display name element (`<name>`).
    pub const NAME: &str = "name";
    /// Battery percentage element (`<battery>`).
    pub const BATTERY: &str = "battery";
    /// Low-battery flag element (`<batterylow>`).
    pub const BATTERY_LOW: &str = "batterylow";
    /// Switch relay block (`<switch>`).
    pub const SWITCH: &str = "switch";
    /// HAN-FUN / DECT 500 simple relay block (`<simpleonoff>`).
    pub const SIMPLE_ON_OFF: &str = "simpleonoff";
    /// Binary alarm/contact sensor block (`<alert>`).
    pub const ALERT: &str = "alert";
    /// Binary state child element (`<state>`).
    pub const STATE: &str = "state";
    /// Temperature sensor block (`<temperature>`).
    pub const TEMPERATURE: &str = "temperature";
    /// Decicelsius value (`<celsius>`).
    pub const CELSIUS: &str = "celsius";
    /// Decicelsius internal FritzBox offset (`<offset>`).
    pub const OFFSET: &str = "offset";
    /// Humidity sensor block (`<humidity>`).
    pub const HUMIDITY: &str = "humidity";
    /// Relative humidity percentage (`<rel_humidity>`).
    pub const REL_HUMIDITY: &str = "rel_humidity";
    /// Radiator thermostat block (`<hkr>`).
    pub const HKR: &str = "hkr";
    /// Measured valve temperature in `0.5 °C` steps (`<tist>`).
    pub const TIST: &str = "tist";
    /// Target valve temperature in `0.5 °C` steps (`<tsoll>`).
    pub const TSOLL: &str = "tsoll";
    /// Open-window active flag (`<windowopenactiv>`).
    pub const WINDOW_OPEN_ACTIVE: &str = "windowopenactiv";
    /// Boost active flag (`<boostactive>`).
    pub const BOOST_ACTIVE: &str = "boostactive";
    /// Numeric HKR error code (`<errorcode>`).
    pub const ERROR_CODE: &str = "errorcode";
    /// Power meter block (`<powermeter>`).
    pub const POWERMETER: &str = "powermeter";
    /// Line voltage in millivolts (`<voltage>`).
    pub const VOLTAGE: &str = "voltage";
    /// Active power in milliwatts (`<power>`).
    pub const POWER: &str = "power";
    /// Cumulative energy in watt-hours (`<energy>`).
    pub const ENERGY: &str = "energy";
    /// ETSI HAN-FUN sub-unit descriptor (`<etsiunitinfo>`).
    pub const ETSI_UNIT_INFO: &str = "etsiunitinfo";
    /// Parent HAN-FUN base device ID (`<etsideviceid>`).
    pub const ETSI_DEVICE_ID: &str = "etsideviceid";
    /// Push-button child element (`<button>`).
    pub const BUTTON: &str = "button";
    /// Unix timestamp of last button press (`<lastpressedtimestamp>`).
    pub const LAST_PRESSED: &str = "lastpressedtimestamp";
}

// =============================================================================
/// FritzBox AHA login & TR-064 SOAP / SCPD protocol constants.
pub mod tr064_defaults {
    /// Default HTTP TCP port for FritzBox TR-064 / UPnP SOAP control.
    pub const TR064_PORT: u16 = 49_000;
    /// Invalid / unauthenticated 16-zero FritzBox Session ID (`SID`).
    pub const INVALID_SID: &str = "0000000000000000";
    /// Maximum accepted PBKDF2 iteration count in v2 login challenges (`DoS` guard).
    pub const MAX_PBKDF2_ITERATIONS: u32 = 1_000_000;
    /// Default manufacturer fallback for FritzBox CPE metadata.
    pub const DEFAULT_MANUFACTURER: &str = "AVM";
    /// Placeholder metadata string when hardware omits model/serial/firmware.
    pub const UNKNOWN_META: &str = "Unknown";
    /// Placeholder WAN addresses treated as unassigned (`0.0.0.0`, `::`, empty).
    pub const INVALID_WAN_IPS: &[&str] = &["", "0.0.0.0", "::"];
    /// Default maximum call-log entries returned on `/api/v1/system/calls`.
    pub const DEFAULT_CALL_LIMIT: usize = 50;
    /// Default phonebook index (`0` = main FritzBox phonebook).
    pub const DEFAULT_PHONEBOOK_ID: u32 = 0;
    /// Candidate SCPD XML description endpoints probed during TR-064 discovery.
    pub const SCPD_DISCOVERY_PATHS: &[&str] = &["/igddesc.xml", "/igd2desc.xml", "/tr64desc.xml"];
}
