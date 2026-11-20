//! Outdoor climate models (`Open-Meteo` current conditions, hourly slots, and 5-day forecast).
//!
//! Provides strongly typed snapshots for `GET /api/v1/weather` (`WeatherData`),
//! `GET /api/v1/state` (`CurrentWeather`), and `WEATHER_01` history serialization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::heating_forecast::ForecastDayInput;
use crate::core::psychrometrics::{calculate_absolute_humidity, calculate_dew_point};

// =============================================================================
/// Live outdoor climate observation enriched with `WMO-No. 8` psychrometrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentWeather {
    /// Outdoor dry-bulb air temperature (`°C`).
    pub temperature: Option<f64>,
    /// Outdoor apparent (feels-like) temperature (`°C`).
    pub apparent_temperature: Option<f64>,
    /// Outdoor relative humidity (`%`).
    pub humidity: Option<f64>,
    /// Calculated outdoor dew point (`°C`).
    pub dew_point_c: Option<f64>,
    /// Calculated outdoor absolute humidity (`g/m³`).
    pub abs_humidity_gm3: Option<f64>,
    /// 10-meter wind speed (`km/h`).
    pub wind_speed: Option<f64>,
    /// `WMO Code 4677` condition code (`0`..=`99`, or `-1` when unknown).
    pub weather_code: i32,
    /// Human-readable WMO condition description (`"Clear sky"`, etc.).
    pub weather_description: String,
    /// Daylight indicator (`1` = day, `0` = night).
    pub is_day: u8,
}

impl CurrentWeather {
    /// Computes and populates `dew_point_c` and `abs_humidity_gm3` when temp & RH are valid.
    pub fn enrich_psychrometrics(&mut self) {
        if let (Some(temp), Some(rh)) = (self.temperature, self.humidity) {
            if rh > 0.0 {
                self.dew_point_c = Some(calculate_dew_point(temp, rh));
                self.abs_humidity_gm3 = Some(calculate_absolute_humidity(temp, rh));
                return;
            }
        }
        self.dew_point_c = None;
        self.abs_humidity_gm3 = None;
    }

    /// Extracts `(temperature_c, abs_humidity_gm3)` for indoor `ventilation_advice` calculation.
    #[must_use]
    pub fn outdoor_psychrometrics(&self) -> (Option<f64>, Option<f64>) {
        (self.temperature, self.abs_humidity_gm3)
    }
}

// =============================================================================
/// Two-hour preview slot exposed in `GET /api/v1/weather` (`hourly` array).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourlyForecastSlot {
    /// Local wall-clock time string (`"HH:MM"`).
    pub time: String,
    /// Forecast air temperature (`°C`).
    pub temperature: Option<f64>,
    /// `WMO Code 4677` condition code.
    pub weather_code: i32,
    /// Human-readable WMO condition description.
    pub weather_description: String,
}

// =============================================================================
/// Daily forecast summary exposed in `GET /api/v1/weather` (`daily` array).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyForecastSlot {
    /// ISO date (`"YYYY-MM-DD"`).
    pub date: String,
    /// `WMO Code 4677` condition code.
    pub weather_code: i32,
    /// Human-readable WMO condition description.
    pub weather_description: String,
    /// Forecast maximum temperature (`°C`).
    pub temp_max: Option<f64>,
    /// Forecast minimum temperature (`°C`).
    pub temp_min: Option<f64>,
    /// Sunrise timestamp (`"YYYY-MM-DDTHH:MM"`).
    pub sunrise: Option<String>,
    /// Sunset timestamp (`"YYYY-MM-DDTHH:MM"`).
    pub sunset: Option<String>,
}

// =============================================================================
/// Complete cached Open-Meteo weather snapshot matching `WeatherData` in `openapi.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    /// Adapter status (`"online"`, `"loading"`, `"offline"`, `"error"`, `"unknown"`).
    pub status: String,
    /// Resolved home IANA timezone (`"Europe/Berlin"`).
    pub timezone: String,
    /// Unix epoch timestamp (`s`) of the last successful Open-Meteo sync.
    pub last_update: Option<f64>,
    /// Current outdoor weather observation.
    pub current: Option<CurrentWeather>,
    /// Upcoming 2-hour forecast slots (`5` entries).
    pub hourly: Vec<HourlyForecastSlot>,
    /// Upcoming 5-day forecast entries.
    pub daily: Vec<DailyForecastSlot>,
}

impl Default for WeatherSnapshot {
    /// Returns an uninitialized `"unknown"` weather snapshot in [`TIMEZONE`].
    fn default() -> Self {
        use crate::core::domain_constants::automation::weather_defaults::TIMEZONE;
        Self {
            status: String::from("unknown"),
            timezone: String::from(TIMEZONE),
            last_update: None,
            current: None,
            hourly: Vec::new(),
            daily: Vec::new(),
        }
    }
}

impl WeatherSnapshot {
    /// Builds the `weather` field (`CurrentWeather | null`) embedded in `GET /api/v1/state`.
    #[must_use]
    pub fn to_state_summary(&self) -> Value {
        match &self.current {
            Some(cur) => serde_json::to_value(cur).unwrap_or(Value::Null),
            None => Value::Null,
        }
    }

    /// Builds the `state_json` payload persisted under `WEATHER_01` in `state_history`.
    #[must_use]
    pub fn to_history_json(&self) -> Option<Value> {
        let cur = self.current.as_ref()?;
        cur.temperature?;
        serde_json::to_value(cur).ok()
    }

    /// Converts `self.daily` into [`ForecastDayInput`] slices for [`crate::core::heating_forecast`].
    #[must_use]
    pub fn to_forecast_inputs(&self) -> Vec<ForecastDayInput> {
        self.daily
            .iter()
            .map(|d| ForecastDayInput {
                date: d.date.clone(),
                temp_min: d.temp_min,
                temp_max: d.temp_max,
            })
            .collect()
    }
}
