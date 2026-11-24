//! Weather, building-physics psychrometrics (`DIN 4108-2`), automation, and LWW sync constants.
//!
//! Centralizes Open-Meteo defaults, WMO Code 4677 labels, Magnus/DIN 4108-2 thermal-bridge
//! thresholds, `AdaptiveStart` (`DIN EN 12098-1`) bounds, and Last-Write-Wins TTL rules.

// =============================================================================
/// Default parameters and variable lists for Open-Meteo weather queries.
pub mod weather_defaults {
    /// Default latitude (Berlin) when unconfigured.
    pub const LATITUDE: f64 = 52.52;
    /// Default longitude (Berlin) when unconfigured.
    pub const LONGITUDE: f64 = 13.41;
    /// Default IANA home timezone for wall-clock schedules and daily energy buckets.
    pub const TIMEZONE: &str = "Europe/Berlin";
    /// Special timezone token instructing Open-Meteo to resolve timezone from coordinates.
    pub const TIMEZONE_AUTO: &str = "auto";
    /// Default weather polling interval in seconds (`1` hour).
    pub const UPDATE_INTERVAL_SECS: u64 = 3600;
    /// Default forecast horizon in days.
    pub const FORECAST_DAYS: u8 = 5;
    /// Maximum forecast horizon accepted by Open-Meteo (`16` days).
    pub const MAX_FORECAST_DAYS: u8 = 16;
    /// Open-Meteo v1 REST API base URL.
    pub const BASE_URL: &str = "https://api.open-meteo.com/v1";
    /// HTTP timeout in seconds for Open-Meteo requests.
    pub const REQUEST_TIMEOUT_SECS: u64 = 15;
    /// Delay in seconds between transient Open-Meteo retry attempts.
    pub const ERROR_RETRY_DELAY_SECS: u64 = 5;
    /// Number of upcoming 2-hour forecast slots exposed in `GET /api/v1/weather`.
    pub const HOURLY_FORECAST_SLOTS: usize = 5;
    /// Step size in hours between hourly forecast preview slots.
    pub const HOURLY_STEP_HOURS: usize = 2;
    /// Sentinel weather code when telemetry is unavailable.
    pub const UNKNOWN_WEATHER_CODE: i32 = -1;
    /// Current-weather query parameters requested from Open-Meteo.
    pub const CURRENT_VARS: &str = "temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m,apparent_temperature,is_day";
    /// Hourly forecast query parameters requested from Open-Meteo.
    pub const HOURLY_VARS: &str = "temperature_2m,weather_code,is_day";
    /// Daily forecast query parameters requested from Open-Meteo.
    pub const DAILY_VARS: &str =
        "weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset";

    /// Maps a `WMO Code 4677` integer (`0`..=`99`) to a concise English description.
    #[must_use]
    pub const fn wmo_description(code: i32) -> &'static str {
        match code {
            0 => "Clear sky",
            1 => "Mostly clear",
            2 => "Partly cloudy",
            3 => "Overcast",
            45 => "Fog",
            48 => "Rime fog",
            51 => "Light drizzle",
            53 => "Moderate drizzle",
            55 => "Heavy drizzle",
            56 => "Light freezing drizzle",
            57 => "Heavy freezing drizzle",
            61 => "Light rain",
            63 => "Moderate rain",
            65 => "Heavy rain",
            66 => "Light freezing rain",
            67 => "Heavy freezing rain",
            71 => "Light snowfall",
            73 => "Moderate snowfall",
            75 => "Heavy snowfall",
            77 => "Snow grains",
            80 => "Light rain showers",
            81 => "Moderate rain showers",
            82 => "Heavy rain showers",
            85 => "Light snow showers",
            86 => "Heavy snow showers",
            95 => "Thunderstorm",
            96 => "Thunderstorm with light hail",
            99 => "Thunderstorm with heavy hail",
            _ => "Unknown",
        }
    }
}

// =============================================================================
/// Standardized feature names, scopes, and weekday keys for the `RulesEngine`.
pub mod automation_identifiers {
    /// Seasonal summer pause with cold-snap exception.
    pub const WEATHER_SMART_HEATING: &str = "WeatherSmartHeating";
    /// `DIN 4108-2` mold & humidity protection (`Humidity Guard`).
    pub const HUMIDITY_CONTROL: &str = "HumidityControl";
    /// House-wide frost protection override with pre-return wakeup.
    pub const VACATION_MODE: &str = "VacationMode";
    /// Master-slave room sensor sync (`FRITZ!Smart Control 440`).
    pub const APPARENT_TEMPERATURE: &str = "ApparentTemperature";
    /// 7-day / 48-slot heating and smart-plug schedule.
    pub const HEATING_SCHEDULE: &str = "HeatingSchedule";
    /// Accepted alias for `HeatingSchedule` on smart switches.
    pub const POWER_SCHEDULE_ALIAS: &str = "PowerSchedule";
    /// `DIN EN 12098-1` predictive pre-heating (`Pre-Heat`).
    pub const ADAPTIVE_START: &str = "AdaptiveStart";
    /// TR-064 Wi-Fi presence auto-away setback.
    pub const PRESENCE_PROFILING: &str = "PresenceProfiling";

    /// Canonical list of all 7 backend automation features.
    pub const ALL_FEATURES: &[&str] = &[
        WEATHER_SMART_HEATING,
        HUMIDITY_CONTROL,
        VACATION_MODE,
        APPARENT_TEMPERATURE,
        HEATING_SCHEDULE,
        ADAPTIVE_START,
        PRESENCE_PROFILING,
    ];

    /// Global house-wide scope identifier.
    pub const SCOPE_GLOBAL: &str = "global";
    /// Room software-group scope identifier.
    pub const SCOPE_GROUP: &str = "group";
    /// Standalone single-device scope identifier.
    pub const SCOPE_DEVICE: &str = "device";
    /// Supported automation scopes in precedence order.
    pub const ALL_SCOPES: &[&str] = &[SCOPE_GLOBAL, SCOPE_GROUP, SCOPE_DEVICE];

    /// Canonical Monday..Sunday keys (`0`..=`6`) inside `HeatingSchedule.days`.
    pub const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

    /// Canonical `HeatingSchedule` JSON key for comfort temperature (`°C`).
    pub const KEY_COMFORT_TEMP: &str = "comfort_temp";
    /// Canonical `HeatingSchedule` JSON key for eco setback temperature (`°C`).
    pub const KEY_ECO_TEMP: &str = "eco_temp";
    /// Device state & action parameter key for comfort temperature (`°C`).
    pub const KEY_COMFORT_TEMPERATURE: &str = "comfort_temperature";
    /// Device state & action parameter key for eco setback temperature (`°C`).
    pub const KEY_ECO_TEMPERATURE: &str = "eco_temperature";
    /// Accepted parameter aliases for comfort temperature in `set_presets`.
    pub const COMFORT_PARAM_KEYS: &[&str] = &[KEY_COMFORT_TEMPERATURE, KEY_COMFORT_TEMP];
    /// Accepted parameter aliases for eco temperature in `set_presets`.
    pub const ECO_PARAM_KEYS: &[&str] = &[KEY_ECO_TEMPERATURE, KEY_ECO_TEMP];
    /// Accepted parameter aliases for `set_comfort` (including `"temperature"`).
    pub const COMFORT_ACTION_KEYS: &[&str] =
        &["temperature", KEY_COMFORT_TEMPERATURE, KEY_COMFORT_TEMP];
    /// Accepted parameter aliases for `set_eco` (including `"temperature"`).
    pub const ECO_ACTION_KEYS: &[&str] = &["temperature", KEY_ECO_TEMPERATURE, KEY_ECO_TEMP];

    /// Legacy `Pre-Heat` boolean aliases migrated into `preheat_enabled`.
    pub const PREHEAT_LEGACY_ENABLED_KEYS: &[&str] = &[
        "early_start_enabled",
        "early_start",
        "adaptive_start_enabled",
    ];
    /// Legacy pre-heat keys stripped when merging `AdaptiveStart` config maps.
    pub const PREHEAT_LEGACY_MERGE_KEYS: &[&str] = &[
        "early_start_enabled",
        "early_start",
        "adaptive_start_enabled",
        "preheat_enabled",
    ];
    /// Deprecated `Pre-Heat` config keys stripped from `HeatingSchedule` & `AdaptiveStart`.
    pub const PREHEAT_LEGACY_STRIP_KEYS: &[&str] = &[
        "early_start_enabled",
        "early_start",
        "adaptive_start_enabled",
        "max_preheat_minutes",
        "preheat_minutes",
    ];
    /// Deprecated `HumidityControl` v1 keys stripped on read/write (`DIN 4108-2` v2).
    pub const HUMIDITY_LEGACY_KEYS: &[&str] = &[
        "hysteresis_percent",
        "hysteresis",
        "sustain_minutes",
        "action_temp",
        "duration_minutes",
    ];
}

// =============================================================================
/// Default parameters and physical thresholds for all 7 automation rules.
pub mod automation_defaults {
    /// Default `RulesEngine` evaluation interval in seconds.
    pub const ENGINE_INTERVAL_SECS: u64 = 60;
    /// Default global vacation mode toggle.
    pub const VACATION_MODE_DEFAULT: bool = false;
    /// Default frost protection target temperature in `°C`.
    pub const FROST_PROTECTION_TEMP: f64 = 8.0;
    /// Default outdoor temperature threshold (`°C`) for `WeatherSmartHeating`.
    pub const OUTDOOR_TEMP_THRESHOLD: f64 = 16.0;
    /// Default `MM-DD` start date of the summer-pause season.
    pub const SUMMER_PAUSE_START_DATE: &str = "05-01";
    /// Default `MM-DD` end date of the summer-pause season.
    pub const SUMMER_PAUSE_END_DATE: &str = "09-30";
    /// Default relative humidity trigger threshold (`%`) for `HumidityControl` v2.
    pub const HUMIDITY_THRESHOLD: f64 = 60.0;
    /// Minimum configurable humidity threshold (`%`).
    pub const HUMIDITY_MIN_THRESHOLD: f64 = 40.0;
    /// Maximum configurable humidity threshold (`%`).
    pub const HUMIDITY_MAX_THRESHOLD: f64 = 90.0;
    /// Dehumidification hysteresis (`%`) on projected base relative humidity.
    pub const HUMIDITY_HYSTERESIS_PERCENT: f64 = 4.0;
    /// Default maximum thermal-shielding boost (`°C`) above `T_base`.
    pub const HUMIDITY_MAX_TEMP_BOOST_C: f64 = 2.0;
    /// Minimum boost (`0.0 °C` = `Alert only` mode without valve boost).
    pub const HUMIDITY_MIN_TEMP_BOOST_C: f64 = 0.0;
    /// Upper validation bound for `max_temp_boost_c` (`°C`).
    pub const HUMIDITY_MAX_TEMP_BOOST_LIMIT_C: f64 = 4.0;
    /// Absolute comfort ceiling (`°C`) for `HumidityControl` thermal shielding.
    pub const HUMIDITY_ACTION_TEMP_CAP: f64 = 23.0;
    /// Transient shower/cooking debounce window in minutes.
    pub const HUMIDITY_SUSTAIN_MINUTES: u64 = 20;
    /// Maximum hold duration in minutes before gentle `-0.5 °C` step-downs begin.
    pub const HUMIDITY_DURATION_MINUTES: u64 = 180;
    /// Default pre-return wakeup lead time in hours for `VacationMode`.
    pub const VACATION_PRE_HEAT_HOURS: u64 = 6;
    /// Lower plausibility bound for learned room heating rate (`min / +1 °C`).
    pub const MIN_HEATING_RATE_MIN_PER_C: f64 = 15.0;
    /// Upper plausibility bound for learned room heating rate (`min / +1 °C`).
    pub const MAX_HEATING_RATE_MIN_PER_C: f64 = 120.0;
    /// Fallback room heating rate (`min / +1 °C`) before sufficient ramps are observed.
    pub const DEFAULT_HEATING_RATE_MIN_PER_C: f64 = 25.0;
    /// Recent `state_history` row limit (`200`) used for fallback heating-ramp extraction.
    pub const HEATING_RAMP_HISTORY_LIMIT: i64 = 200;
    /// Minimum active target-over-indoor demand (`0.5 °C`) during ongoing ramp steps.
    pub const HEATING_RAMP_MIN_DEMAND_DELTA: f64 = 0.5;
    /// Full temperature gain (`0.5 °C`) completing a heating ramp step immediately.
    pub const HEATING_RAMP_MIN_TEMP_GAIN: f64 = 0.5;
    /// Minimum final temperature gain (`0.3 °C`) accepted when demand ends at setpoint.
    pub const HEATING_RAMP_MIN_FINAL_GAIN: f64 = 0.3;
    /// Minimum ramp sample count (`5`) before trimming min/max outliers.
    pub const HEATING_RAMP_TRIM_MIN_SAMPLES: usize = 5;
    /// Maximum accepted/skipped ramp entries exposed in AI diagnostics (`30`).
    pub const MAX_DIAGNOSTIC_RAMPS: usize = 30;
    /// Boiler burner dead-time cutoff in minutes (`DIN EN 15232`).
    pub const DEAD_TIME_THRESHOLD_MINUTES: f64 = 25.0;
    /// Minimum radiator-over-room delta (`°C`) indicating active hot-water circulation.
    pub const VALVE_WARM_DELTA_C: f64 = 0.5;
    /// Maximum `DIN EN 12098-1` pre-heat lead time in minutes (`3` hours).
    pub const PREHEAT_MAX_MINUTES: i64 = 180;
    /// Minimum indoor deficit (`K`) required to latch a new `Pre-Heat` cycle.
    pub const PREHEAT_MIN_DELTA_C: f64 = 0.5;
    /// Reference outdoor temperature (`°C`) for thermal-inertia scaling.
    pub const PREHEAT_REF_OUTDOOR_TEMP_C: f64 = 10.0;
    /// Thermal-inertia increase per Kelvin below `PREHEAT_REF_OUTDOOR_TEMP_C` (`+2.5 %/K`).
    pub const PREHEAT_OUTDOOR_INERTIA_PER_K: f64 = 0.025;
    /// Minimum outdoor inertia multiplier.
    pub const PREHEAT_MIN_OUTDOOR_FACTOR: f64 = 0.70;
    /// Maximum outdoor inertia multiplier.
    pub const PREHEAT_MAX_OUTDOOR_FACTOR: f64 = 1.60;
    /// Default Wi-Fi auto-away temperature setback (`°C`).
    pub const PRESENCE_AWAY_TEMP_OFFSET: f64 = -3.0;
    /// Default Wi-Fi offline grace period in minutes before auto-away activates.
    pub const PRESENCE_TIMEOUT_MINUTES: u64 = 15;
    /// Default maximum dynamic room-sensor compensation offset (`±4.0 °C`).
    pub const MAX_DYNAMIC_TEMP_OFFSET: f64 = 4.0;
    /// Default `HeatingSchedule` eco/setback temperature (`°C`).
    pub const SCHEDULE_DEFAULT_ECO_TEMP: f64 = 18.0;
    /// Default `HeatingSchedule` comfort temperature (`°C`).
    pub const SCHEDULE_DEFAULT_COMFORT_TEMP: f64 = 22.0;
    /// Granularity of weekly schedule slots in minutes.
    pub const SCHEDULE_SLOT_MINUTES: u16 = 30;
    /// Minutes per standard `24 h` day (`1440`) for `HH:MM` schedule slot arithmetic.
    pub const MINUTES_PER_DAY_U16: u16 = 1440;
    /// In-memory climate sample retention window in seconds (`30 min`) for `HumidityControl`.
    pub const HUMIDITY_SAMPLE_RETENTION_SECS: f64 = 1800.0;
    /// Gentle recovery step-down interval in seconds (`30 min`) after `HUMIDITY_DURATION_MINUTES`.
    pub const HUMIDITY_STEP_DOWN_INTERVAL_SECS: f64 = 1800.0;
    /// Gentle recovery step-down decrement (`0.5 °C`).
    pub const HUMIDITY_STEP_DOWN_DELTA_C: f64 = 0.5;
    /// 15-minute relative humidity jump (`+5.0 %`) classifying a transient shower/cooking spike.
    pub const HUMIDITY_TRANSIENT_RH_JUMP_PCT: f64 = 5.0;
    /// 15-minute absolute humidity jump (`+1.5 g/m³`) classifying a transient spike.
    pub const HUMIDITY_TRANSIENT_ABS_JUMP_GM3: f64 = 1.5;
    /// 10-minute indoor cooling drop (`-1.0 °C`) suppressing shielding during window ventilation.
    pub const HUMIDITY_WINDOW_DROP_DELTA_C: f64 = -1.0;
}

// =============================================================================
/// Building-physics psychrometric constants (`WMO-No. 8` Magnus & `DIN 4108-2`).
pub mod psychrometrics {
    /// Magnus saturation vapor pressure reference at `0 °C` (`hPa`).
    pub const MAGNUS_E0_HPA: f64 = 6.112;
    /// Magnus dimensionless water-over-liquid coefficient.
    pub const MAGNUS_A: f64 = 17.62;
    /// Magnus temperature offset in `°C`.
    pub const MAGNUS_B_C: f64 = 243.12;
    /// Gas-law conversion factor yielding absolute humidity in `g/m³`.
    pub const ABS_HUMIDITY_FACTOR: f64 = 216.7;
    /// Zero Celsius in Kelvin (`K`).
    pub const KELVIN_OFFSET: f64 = 273.15;
    /// `DIN 4108-2` critical thermal-bridge temperature factor (`f_Rsi`).
    pub const DIN_4108_FRSI: f64 = 0.70;
    /// Target safe thermal-bridge surface relative humidity (`%`).
    pub const WALL_RH_TARGET_MAX_PCT: f64 = 75.0;
    /// `DIN 4108-2` mold germination limit on thermal bridges (`80 %` RH).
    pub const WALL_RH_MOLD_LIMIT_PCT: f64 = 80.0;
    /// Safe surface humidity threshold (`%`) permitting gentle `30 min` step-downs.
    pub const WALL_RH_STEPDOWN_SAFE_PCT: f64 = 78.0;
    /// Absolute-humidity advantage (`g/m³`) recommending immediate ventilation.
    pub const VENT_DELTA_NOW_GM3: f64 = 1.5;
    /// Minimum absolute-humidity advantage (`g/m³`) for marginal ventilation benefit.
    pub const VENT_DELTA_MARGINAL_GM3: f64 = 0.5;
    /// Australian Apparent Temperature vapor-pressure coefficient (`6.105`).
    pub const APPARENT_VAPOUR_COEFF: f64 = 6.105;
    /// Australian Apparent Temperature exponent numerator (`17.27`).
    pub const APPARENT_TEMP_FACTOR: f64 = 17.27;
    /// Australian Apparent Temperature exponent offset (`237.7`).
    pub const APPARENT_TEMP_OFFSET: f64 = 237.7;
    /// Australian Apparent Temperature humidity weight (`0.33`).
    pub const APPARENT_HUMIDITY_COEFF: f64 = 0.33;
    /// Australian Apparent Temperature windless indoor constant (`4.0`).
    pub const APPARENT_BASE_SUBTRACTION: f64 = 4.0;

    /// No ventilation required.
    pub const ADVICE_NONE: &str = "none";
    /// Outdoor air is significantly drier (`>= 1.5 g/m³` advantage).
    pub const ADVICE_VENTILATE_NOW: &str = "ventilate_now";
    /// Outdoor air is moderately drier (`0.5..1.5 g/m³` advantage).
    pub const ADVICE_VENTILATE_MARGINAL: &str = "ventilate_marginal";
    /// Outdoor air is too humid to dry the room (`< 0.5 g/m³` advantage).
    pub const ADVICE_DO_NOT_VENTILATE: &str = "do_not_ventilate_humid_outside";

    /// Minimum vapor pressure (`hPa`) avoiding division-by-zero or `ln(<= 0)`.
    pub const MIN_VAPOR_PRESSURE_HPA: f64 = 0.001;
    /// Minimum clamped relative humidity (`%`).
    pub const MIN_REL_HUMIDITY_PCT: f64 = 0.1;
    /// Maximum clamped relative humidity (`%`).
    pub const MAX_REL_HUMIDITY_PCT: f64 = 100.0;
    /// Minimum target relative humidity (`%`) when inverting Magnus for `T_guard`.
    pub const MIN_TARGET_RH_PCT: f64 = 10.0;
    /// Minimum thermal buoyancy delta (`K`) above outdoor temperature for `T_guard`.
    pub const THERMAL_BUOYANCY_DELTA_K: f64 = 2.0;
    /// 15-minute transient shower/cooking spike window in seconds (`900 s`).
    pub const TRANSIENT_WINDOW_SECS: f64 = 900.0;
    /// 10-minute open-window cooling/drying detection window in seconds (`600 s`).
    pub const WINDOW_DROP_WINDOW_SECS: f64 = 600.0;
    /// Absolute humidity drop (`g/m³`) within `10 min` indicating active window ventilation.
    pub const WINDOW_DROP_ABS_GM3: f64 = -1.0;
    /// Temperature rebound (`°C`) cancelling open-window suppression after brief drafts.
    pub const WINDOW_REBOUND_DELTA_C: f64 = 0.2;
}

// =============================================================================
/// Canonical `preheat_paused_reason` enumeration values matching `assets/openapi.yaml`.
pub mod preheat_paused_reasons {
    /// Paused because house-wide `VacationMode` is locking the thermostat.
    pub const VACATION_MODE: &str = "vacation_mode";
    /// Paused because `WeatherSmartHeating` (`Summer Pause`) is suppressing heating.
    pub const SUMMER_PAUSE: &str = "summer_pause";
    /// Paused because `PresenceProfiling` (`Auto-Away`) setback is active.
    pub const AUTO_AWAY: &str = "auto_away";
    /// Paused because the radiator reports an open window (`window_open == true`).
    pub const WINDOW_OPEN: &str = "window_open";
    /// Paused because a manual user setpoint override is latched for this phase.
    pub const MANUAL_OVERRIDE: &str = "manual_override";
}

// =============================================================================
/// Canonical 4-state identifiers for `HumidityControl` v2 (`DIN 4108-2`).
pub mod humidity_guard_states {
    /// Humidity is within safe limits.
    pub const IDLE: &str = "IDLE";
    /// High humidity detected; waiting out 20-minute shower/cooking debounce or window draft.
    pub const TRANSIENT_DEBOUNCE: &str = "TRANSIENT_DEBOUNCE";
    /// Active `T_guard` thermal-bridge shielding in progress.
    pub const THERMAL_SHIELDING: &str = "THERMAL_SHIELDING";
    /// Gentle 30-minute `-0.5 °C` recovery step-down check after 180 minutes.
    pub const RECOVERY_CHECK: &str = "RECOVERY_CHECK";
}

// =============================================================================
/// Multi-device offline-sync, Last-Write-Wins (LWW), and Command TTL constants.
pub mod sync_defaults {
    use crate::core::domain_constants::hardware::{device_actions, router_actions};

    /// Threshold distinguishing Unix seconds from Java/Kotlin millisecond timestamps.
    pub const MILLISECOND_THRESHOLD: f64 = 100_000_000_000.0;
    /// Maximum age in seconds (`5` minutes) accepted for direct physical switch/router actions.
    pub const COMMAND_TTL_SECONDS: f64 = 300.0;
    /// DECT-ULE target hold window in seconds (`90 s`) after backend target changes.
    pub const TARGET_HOLD_SECONDS: f64 = 90.0;
    /// Actions subject to strict `COMMAND_TTL_SECONDS` expiration.
    pub const TTL_PROTECTED_ACTIONS: &[&str] = &[
        device_actions::SET_SWITCH,
        device_actions::TOGGLE_SWITCH,
        device_actions::SET_BLIND,
        router_actions::REBOOT,
        router_actions::RECONNECT,
        router_actions::WAKE_ON_LAN,
    ];
    /// Manual actuator commands latching an active-slot schedule override in `RulesEngine`.
    pub const MANUAL_OVERRIDE_ACTIONS: &[&str] = &[
        device_actions::SET_TEMPERATURE,
        device_actions::SET_COMFORT,
        device_actions::SET_ECO,
        device_actions::SET_SWITCH,
        device_actions::TOGGLE_SWITCH,
    ];
}
