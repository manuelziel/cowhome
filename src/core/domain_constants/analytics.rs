//! Time-series history compression, thermal efficiency (`VDI 2067/3807`), and smart energy constants.
//!
//! Defines deadband thresholds, `30`-minute heartbeat and pre-edge anchor intervals,
//! 4-factor heating efficiency weights, 5-day HDD forecast bounds, and `EnWG § 40` tariff limits.

// =============================================================================
/// Time-series recording deadbands, heartbeat intervals, and column definitions.
pub mod history_defaults {
    /// Default time-range filter when `range` is omitted on `GET /api/v1/history`.
    pub const DEFAULT_RANGE: &str = "24h";
    /// Token requesting full unbounded history (`range=all`).
    pub const RANGE_ALL: &str = "all";
    /// Default maximum data points returned per query (`0` = unlimited).
    pub const DEFAULT_LIMIT: usize = 5_000;
    /// Upper cap on explicit positive `limit` values.
    pub const MAX_LIMIT: usize = 100_000;
    /// Default retention period in days before daily `00:00` auto-pruning removes rows.
    pub const DEFAULT_RETENTION_DAYS: u32 = 365;
    /// Supported `range` tokens on `GET /api/v1/history`.
    pub const SUPPORTED_RANGES: &[&str] = &["24h", "7d", "30d", "90d", "180d", "365d", "1y", "all"];

    /// Maximum silence interval (`30` minutes) before a heartbeat snapshot is written.
    pub const HEARTBEAT_INTERVAL_SECS: f64 = 1800.0;
    /// Minimum suppressed gap (`120 s`) triggering a pre-edge step-before anchor point.
    pub const PRE_EDGE_MIN_GAP_SECS: f64 = 120.0;
    /// Floating-point epsilon preventing rounding jitter near deadband boundaries.
    pub const FLOAT_EPSILON: f64 = 1e-6;
    /// Measured indoor/hardware temperature deadband (`0.1 °C`).
    pub const DEADBAND_TEMP_C: f64 = 0.1;
    /// Target / preset / offset temperature deadband (`0.5 °C`).
    pub const DEADBAND_TARGET_TEMP_C: f64 = 0.5;
    /// Relative humidity deadband (`1.0 %`).
    pub const DEADBAND_HUMIDITY_PCT: f64 = 1.0;
    /// Battery charge level deadband (`1.0 %`).
    pub const DEADBAND_BATTERY_PCT: f64 = 1.0;
    /// Mains voltage deadband (`3.0 V`) filtering grid fluctuations.
    pub const DEADBAND_VOLTAGE_V: f64 = 3.0;
    /// Minimum absolute active power deadband (`0.5 W`).
    pub const DEADBAND_POWER_WATTS: f64 = 0.5;
    /// Relative active power deadband (`2 %` of previous reading).
    pub const DEADBAND_POWER_RATIO: f64 = 0.02;
    /// Cumulative energy counter deadband (`10.0 Wh`).
    pub const DEADBAND_ENERGY_WH: f64 = 10.0;

    /// Column-oriented schema returned for `thermostat` and `thermostat_group` history.
    pub const THERMOSTAT_COLUMNS: &[&str] = &[
        "timestamp",
        "temperature_current",
        "temperature_target",
        "humidity",
        "battery_level",
        "window_open",
        "temperature_hardware",
        "hardware_target_temperature",
        "humidity_guard_active",
        "preheat_active",
    ];
    /// Column-oriented schema returned for dedicated climate `sensor` history (`DECT 440`).
    pub const SENSOR_COLUMNS: &[&str] = &[
        "timestamp",
        "temperature_current",
        "humidity",
        "battery_level",
    ];
    /// Column-oriented schema returned for `switch` and `power_meter` history.
    pub const SWITCH_COLUMNS: &[&str] = &[
        "timestamp",
        "state",
        "power_current",
        "energy_total",
        "temperature_current",
        "voltage",
    ];
    /// Column-oriented schema returned for `WEATHER_01` outdoor climate history.
    pub const WEATHER_COLUMNS: &[&str] = &[
        "timestamp",
        "temperature",
        "apparent_temperature",
        "humidity",
        "wind_speed",
        "weather_code",
    ];
    /// Column-oriented schema returned for `ROUTER_01` availability history.
    pub const ROUTER_COLUMNS: &[&str] = &["timestamp", "present"];
}

// =============================================================================
/// Weights and bounds for the 4-factor Thermal Heating Efficiency Score & 5-day HDD forecast.
pub mod efficiency_defaults {
    /// Seconds per minute (`60.0`).
    pub const SECONDS_PER_MINUTE: f64 = 60.0;
    /// Minutes per hour (`60.0`).
    pub const MINUTES_PER_HOUR: f64 = 60.0;
    /// Minutes per standard `24 h` day (`1440.0`).
    pub const MINUTES_PER_DAY: f64 = 1440.0;
    /// Milliseconds per second (`1000.0`).
    pub const MILLIS_PER_SECOND: f64 = 1000.0;
    /// Seconds per standard `24 h` day.
    pub const SECONDS_PER_DAY: f64 = 86_400.0;
    /// Minimum observed time span (`1 h` = `1/24 d`) for daily normalization.
    pub const MIN_DAYS_SPAN: f64 = 1.0 / 24.0;
    /// Fallback integration step (`60 s`) for isolated boundary samples.
    pub const FALLBACK_STEP_SECS: f64 = 60.0;
    /// Target-over-current delta (`0.5 °C`) indicating active heating demand.
    pub const DEMAND_DELTA_THRESHOLD_C: f64 = 0.5;
    /// Current-over-target delta (`0.5 °C`) indicating room temperature overshoot.
    pub const OVERSHOOT_DELTA_THRESHOLD_C: f64 = 0.5;
    /// Minimum indoor-to-outdoor delta (`1.0 K`) used as HDD denominator floor.
    pub const MIN_TEMP_DIFF_OUTDOOR_K: f64 = 1.0;
    /// Ideal heating demand intensity (`10.0 min / (K * d)`).
    pub const IDEAL_DEMAND_INTENSITY: f64 = 10.0;
    /// Demand intensity span (`50.0 min / (K * d)`) over which `demand_score` decays.
    pub const DEMAND_INTENSITY_SPAN: f64 = 50.0;
    /// Baseline `setback_score` when zero night/away setback is configured.
    pub const BASE_SETBACK_SCORE: f64 = 0.50;
    /// Additional `setback_score` span earned up to `IDEAL_SETBACK_DELTA_K`.
    pub const SETBACK_SCORE_SPAN: f64 = 0.50;
    /// Average daily setback delta (`1.5 K`) achieving `1.0` `setback_score`.
    pub const IDEAL_SETBACK_DELTA_K: f64 = 1.5;
    /// Rule-of-thumb energy savings per Kelvin setback (`6.0 %/K`, `VDI 2067`).
    pub const VDI_SAVINGS_PER_KELVIN: f64 = 6.0;
    /// Upper cap on estimated setback savings (`30.0 %`).
    pub const MAX_SAVINGS_PERCENT: f64 = 30.0;
    /// Ideal heating ramp speed (`20.0 min / °C`).
    pub const IDEAL_HEATING_RATE: f64 = 20.0;
    /// Heating rate span (`80.0 min / °C`) for `recovery_score`.
    pub const HEATING_RATE_SPAN: f64 = 80.0;
    /// Ideal maximum room setback drop (`4.0 K`) avoiding deep structural cooling.
    pub const IDEAL_MAX_DROP_K: f64 = 4.0;
    /// Drop span (`6.0 K`) above `IDEAL_MAX_DROP_K` penalizing `recovery_score`.
    pub const MAX_DROP_SPAN_K: f64 = 6.0;
    /// Ideal recovery duration (`60.0 min`) from room setback.
    pub const IDEAL_RECOVERY_MINUTES: f64 = 60.0;
    /// Recovery duration span (`120.0 min`) above `IDEAL_RECOVERY_MINUTES`.
    pub const RECOVERY_MINUTES_SPAN: f64 = 120.0;
    /// Floor (`0.20`) applied to recovery rate and drop sub-scores.
    pub const MIN_RECOVERY_SUBSCORE: f64 = 0.20;
    /// Weight (`0.60`) of ramp speed inside `recovery_score`.
    pub const RECOVERY_RATE_WEIGHT: f64 = 0.60;
    /// Weight (`0.40`) of setback drop depth inside `recovery_score`.
    pub const RECOVERY_DROP_WEIGHT: f64 = 0.40;
    /// Historical 7-day baseline window (`7.0 d`) for HDD forecast comparison.
    pub const FORECAST_BASELINE_DAYS: f64 = 7.0;
    /// Extended 14-day fallback window (`14.0 d`) when 7d had zero heating demand.
    pub const FORECAST_SIGNATURE_DAYS: f64 = 14.0;
    /// Minimum indoor-to-outdoor delta (`1.0 K`) for learned building signature.
    pub const MIN_BASELINE_HDD_KELVIN: f64 = 1.0;
    /// Penalty per minute of open window during active heating (`0.02 / min`).
    pub const WINDOW_PENALTY_PER_MIN: f64 = 0.02;
    /// Penalty per minute of room temperature overshoot (`0.005 / min`).
    pub const OVERSHOOT_PENALTY_PER_MIN: f64 = 0.005;
    /// Weight of `demand_score` (`35 %`) in composite efficiency score.
    pub const WEIGHT_DEMAND: f64 = 0.35;
    /// Weight of `setback_score` (`25 %`) in composite efficiency score.
    pub const WEIGHT_SETBACK: f64 = 0.25;
    /// Weight of `recovery_score` (`20 %`) in composite efficiency score.
    pub const WEIGHT_RECOVERY: f64 = 0.20;
    /// Weight of `precision_score` (`20 %`) in composite efficiency score.
    pub const WEIGHT_PRECISION: f64 = 0.20;

    /// Rising heating-demand trend label.
    pub const TREND_RISING: &str = "rising";
    /// Falling heating-demand trend label.
    pub const TREND_FALLING: &str = "falling";
    /// Stable heating-demand trend label.
    pub const TREND_STABLE: &str = "stable";
    /// Relative deadband (`±5.0 %`) before classifying a forecast as rising/falling.
    pub const TREND_DEADBAND_PERCENT: f64 = 5.0;
    /// Absolute daily deadband (`±10.0 min/d`) before classifying a forecast trend.
    pub const TREND_DEADBAND_MINUTES: f64 = 10.0;
    /// Lower clamp bound (`2.0 min/(K*d)`) for learned building demand signature.
    pub const MIN_LEARNED_INTENSITY: f64 = 2.0;
    /// Upper clamp bound (`60.0 min/(K*d)`) for learned building demand signature.
    pub const MAX_LEARNED_INTENSITY: f64 = 60.0;
    /// Maximum target temperature (`°C`) considered frost-guard / idle in 5-day forecasts.
    pub const FROST_IDLE_MAX_TEMP: f64 = 8.5;
    /// Minimum active daily heating demand (`min/d`) below which forecast trend stays `"stable"`.
    pub const MIN_ACTIVE_DEMAND_MINUTES: f64 = 5.0;
    /// Baseline denominator floor (`min/d`) preventing percentage blow-up on mild days.
    pub const MIN_BASELINE_DEMAND_MINUTES: f64 = 15.0;
    /// Minimum clamped forecast/energy trend percentage (`-100.0 %`).
    pub const TREND_MIN_PERCENT: f64 = -100.0;
    /// Maximum clamped forecast/energy trend percentage (`+200.0 %`).
    pub const TREND_MAX_PERCENT: f64 = 200.0;
    /// Recommended optimal comfort setpoint (`21.0 °C`) for `VDI 2067` savings estimation.
    pub const RECOMMENDED_COMFORT_TEMP: f64 = 21.0;
    /// Recommended optimal eco setback (`18.0 °C`) for `VDI 2067` savings estimation.
    pub const RECOMMENDED_ECO_TEMP: f64 = 18.0;
    /// Reference daily comfort hours (`14.0 h`) for `VDI 2067` savings estimation.
    pub const RECOMMENDED_COMFORT_HOURS: f64 = 14.0;
    /// Reference daily eco setback hours (`10.0 h`) for `VDI 2067` savings estimation.
    pub const RECOMMENDED_ECO_HOURS: f64 = 10.0;
}

// =============================================================================
/// Smart Energy metering (`DIN EN IEC 62053-21`) and tariff proration (`EnWG § 40`) constants.
pub mod energy_defaults {
    /// `system_settings` key storing working electricity price (`EUR/kWh`).
    pub const SETTING_KEY_PRICE: &str = "electricity_price_eur_per_kwh";
    /// Default working electricity price (`0.35 EUR/kWh`).
    pub const DEFAULT_PRICE_EUR_PER_KWH: f64 = 0.35;
    /// Minimum allowed electricity price (`0.0 EUR/kWh`).
    pub const MIN_PRICE_EUR_PER_KWH: f64 = 0.0;
    /// Maximum allowed electricity price (`5.0 EUR/kWh`).
    pub const MAX_PRICE_EUR_PER_KWH: f64 = 5.0;
    /// `system_settings` key storing monthly base meter fee (`EUR/month`).
    pub const SETTING_KEY_BASE_FEE: &str = "electricity_base_fee_eur_per_month";
    /// Default monthly base fee (`0.00 EUR/month`).
    pub const DEFAULT_BASE_FEE_EUR_PER_MONTH: f64 = 0.0;
    /// Minimum allowed monthly base fee (`0.00 EUR/month`).
    pub const MIN_BASE_FEE_EUR_PER_MONTH: f64 = 0.0;
    /// Maximum allowed monthly base fee (`500.00 EUR/month`).
    pub const MAX_BASE_FEE_EUR_PER_MONTH: f64 = 500.0;
    /// Months per year for annual base-fee proration (`12.0`).
    pub const MONTHS_PER_YEAR: f64 = 12.0;
    /// Synthetic device identifier for house-wide energy summaries (`"house"`).
    pub const IDENTIFIER_HOUSE: &str = "house";
    /// ISO 4217 currency code (`"EUR"`).
    pub const CURRENCY: &str = "EUR";
    /// Watt-hours per kilowatt-hour (`1000.0`).
    pub const WH_PER_KWH: f64 = 1000.0;
    /// Standard 30-day billing month used for daily/weekly base-fee proration.
    pub const MONTH_DAYS: f64 = 30.0;
    /// Standard 365-day year used for annual energy projection.
    pub const YEAR_DAYS: f64 = 365.0;
    /// Minimum observed history (`6 h` = `0.25 d`) required before projecting 30d/365d totals.
    pub const MIN_OBSERVED_DAYS: f64 = 0.25;
    /// Counter jitter tolerance (`1.0 Wh`) below which negative dips are ignored vs. treated as resets.
    pub const COUNTER_RESET_TOLERANCE_WH: f64 = 1.0;
    /// Substring hint identifying optical main grid meters (`FRITZ!Smart Energy 250`).
    pub const GRID_METER_PRODUCT_HINT: &str = "250";
    /// Days per standard week (`7.0`).
    pub const WEEK_DAYS: f64 = 7.0;
    /// Minimum active daily consumption (`kWh/d`) below which trend stays `"stable"`.
    pub const MIN_ACTIVE_DAILY_KWH: f64 = 0.02;
    /// Denominator floor (`kWh/d`) preventing percentage blow-up on idle devices.
    pub const MIN_BASELINE_DAILY_KWH: f64 = 0.05;
    /// Absolute daily consumption deadband (`kWh/d`) for trend stability.
    pub const TREND_DEADBAND_KWH_DAILY: f64 = 0.05;
    /// Accepted JSON request keys for electricity price (`EUR/kWh`) on `/system/energy`.
    pub const PRICE_PAYLOAD_KEYS: &[&str] = &[
        SETTING_KEY_PRICE,
        "tariff_eur_per_kwh",
        "price_eur_per_kwh",
        "electricity_price",
        "price",
    ];
    /// Accepted JSON request keys for monthly base fee (`EUR/month`) on `/system/energy`.
    pub const BASE_FEE_PAYLOAD_KEYS: &[&str] = &[
        SETTING_KEY_BASE_FEE,
        "monthly_base_fee",
        "base_fee_eur_per_month",
        "electricity_base_fee",
        "base_fee",
    ];
    /// Accepted working-price keys in imported SQLite `system_settings` backup objects.
    pub const BACKUP_PRICE_KEYS: &[&str] = &[SETTING_KEY_PRICE, "tariff_eur_per_kwh"];
    /// Accepted monthly base-fee keys in imported SQLite `system_settings` backup objects.
    pub const BACKUP_BASE_FEE_KEYS: &[&str] = &[SETTING_KEY_BASE_FEE, "monthly_base_fee"];
}
