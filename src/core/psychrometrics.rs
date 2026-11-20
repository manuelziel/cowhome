//! Building-physics psychrometrics (`WMO-No. 8` Magnus & `DIN 4108-2` thermal bridges).
//!
//! Pure mathematical functions for saturation vapor pressure, dew point, absolute humidity,
//! critical interior wall surface humidity (`f_Rsi = 0.70`), projected base relative humidity,
//! demand-exact guard temperatures (`T_guard`), apparent temperature, and ventilation advice.

use crate::core::domain_constants::{
    analytics::history_defaults::FLOAT_EPSILON,
    automation::{
        automation_defaults::{
            HUMIDITY_ACTION_TEMP_CAP, HUMIDITY_HYSTERESIS_PERCENT, HUMIDITY_MAX_TEMP_BOOST_C,
            HUMIDITY_THRESHOLD,
        },
        psychrometrics::{
            ABS_HUMIDITY_FACTOR, ADVICE_DO_NOT_VENTILATE, ADVICE_NONE, ADVICE_VENTILATE_MARGINAL,
            ADVICE_VENTILATE_NOW, APPARENT_BASE_SUBTRACTION, APPARENT_HUMIDITY_COEFF,
            APPARENT_TEMP_FACTOR, APPARENT_TEMP_OFFSET, APPARENT_VAPOUR_COEFF, DIN_4108_FRSI,
            KELVIN_OFFSET, MAGNUS_A, MAGNUS_B_C, MAGNUS_E0_HPA, MAX_REL_HUMIDITY_PCT,
            MIN_REL_HUMIDITY_PCT, MIN_TARGET_RH_PCT, MIN_VAPOR_PRESSURE_HPA,
            THERMAL_BUOYANCY_DELTA_K, TRANSIENT_WINDOW_SECS, VENT_DELTA_MARGINAL_GM3,
            VENT_DELTA_NOW_GM3, WALL_RH_TARGET_MAX_PCT, WINDOW_DROP_ABS_GM3,
            WINDOW_DROP_WINDOW_SECS, WINDOW_REBOUND_DELTA_C,
        },
    },
    hardware::fritz_api::{HALF_DEGREE_FACTOR, MAX_THERMOSTAT_TEMP, MIN_THERMOSTAT_TEMP},
};

// =============================================================================
/// Rounds a finite `f64` value to `decimals` fractional digits (`1` -> `0.1`, `2` -> `0.01`).
#[must_use]
pub fn round_decimals(value: f64, decimals: i32) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let scale = 10.0_f64.powi(decimals);
    (value * scale).round() / scale
}

// =============================================================================
/// Calculates indoor apparent (feels-like) temperature (`°C`) via the Australian AT formula.
#[must_use]
pub fn calculate_apparent_temperature(temp_c: f64, humidity_pct: f64) -> f64 {
    if humidity_pct <= 0.0 || temp_c <= 0.0 {
        return round_decimals(temp_c, 1);
    }
    let vp_factor = (humidity_pct / 100.0) * APPARENT_VAPOUR_COEFF;
    let vp = vp_factor * ((APPARENT_TEMP_FACTOR * temp_c) / (APPARENT_TEMP_OFFSET + temp_c)).exp();
    let at = temp_c + APPARENT_HUMIDITY_COEFF * vp - APPARENT_BASE_SUBTRACTION;
    round_decimals(at, 1)
}

// =============================================================================
/// Calculates compensated hardware valve target (`8.0`..=`28.0 °C` in `0.5 °C` steps) for `set_offset`.
#[must_use]
pub fn calculate_static_offset_target(user_target: f64, offset: f64) -> f64 {
    if user_target <= MIN_THERMOSTAT_TEMP {
        return user_target;
    }
    let raw = user_target - offset;
    let rounded = (raw * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR;
    rounded.clamp(MIN_THERMOSTAT_TEMP, MAX_THERMOSTAT_TEMP)
}

// =============================================================================
/// Calculates saturation vapor pressure $E_{\text{sat}}(T)$ (`hPa`) via WMO Magnus.
#[must_use]
pub fn calculate_saturation_vapor_pressure(temp_c: f64) -> f64 {
    let denom = (MAGNUS_B_C + temp_c).max(MIN_VAPOR_PRESSURE_HPA);
    let exponent = (MAGNUS_A * temp_c) / denom;
    MAGNUS_E0_HPA * exponent.exp()
}

// =============================================================================
/// Calculates partial water vapor pressure $E(T, \varphi)$ (`hPa`).
#[must_use]
pub fn calculate_vapor_pressure(temp_c: f64, rel_humidity_pct: f64) -> f64 {
    let clamped_rh = rel_humidity_pct.clamp(MIN_REL_HUMIDITY_PCT, MAX_REL_HUMIDITY_PCT);
    (clamped_rh / 100.0) * calculate_saturation_vapor_pressure(temp_c)
}

// =============================================================================
/// Inverts the WMO Magnus equation to find saturation/dew-point temperature (`°C`).
#[must_use]
pub fn calculate_temperature_from_vapor_pressure(vapor_pressure_hpa: f64) -> f64 {
    let safe_vp = vapor_pressure_hpa.max(MIN_VAPOR_PRESSURE_HPA);
    let gamma = (safe_vp / MAGNUS_E0_HPA).ln();
    let denom = (MAGNUS_A - gamma).max(MIN_VAPOR_PRESSURE_HPA);
    (MAGNUS_B_C * gamma) / denom
}

// =============================================================================
/// Calculates dew point temperature $T_{\text{dew}}(T, \varphi)$ (`°C`, 1 decimal).
#[must_use]
pub fn calculate_dew_point(temp_c: f64, rel_humidity_pct: f64) -> f64 {
    let vp = calculate_vapor_pressure(temp_c, rel_humidity_pct);
    round_decimals(calculate_temperature_from_vapor_pressure(vp), 1)
}

// =============================================================================
/// Calculates absolute humidity $x(T, \varphi)$ in `g/m³` (2 decimals).
#[must_use]
pub fn calculate_absolute_humidity(temp_c: f64, rel_humidity_pct: f64) -> f64 {
    let vp = calculate_vapor_pressure(temp_c, rel_humidity_pct);
    let kelvin = (KELVIN_OFFSET + temp_c).max(MIN_VAPOR_PRESSURE_HPA);
    round_decimals(ABS_HUMIDITY_FACTOR * (vp / kelvin), 2)
}

// =============================================================================
/// Calculates interior wall surface temperature $T_{\text{si}}$ at thermal bridges (`DIN 4108-2`).
#[must_use]
pub fn calculate_wall_surface_temperature(temp_in_c: f64, temp_out_c: f64, f_rsi: f64) -> f64 {
    temp_out_c + f_rsi * (temp_in_c - temp_out_c)
}

// =============================================================================
/// Calculates critical thermal-bridge surface relative humidity $\varphi_{\text{si}}$ (`%`, `DIN 4108-2`).
#[must_use]
pub fn calculate_wall_surface_humidity(
    temp_in_c: f64,
    rel_humidity_in_pct: f64,
    temp_out_c: f64,
    f_rsi: f64,
) -> f64 {
    let t_si = calculate_wall_surface_temperature(temp_in_c, temp_out_c, f_rsi);
    let vp_in = calculate_vapor_pressure(temp_in_c, rel_humidity_in_pct);
    let e_sat_si = calculate_saturation_vapor_pressure(t_si).max(MIN_VAPOR_PRESSURE_HPA);
    let raw_pct = 100.0 * (vp_in / e_sat_si);
    round_decimals(raw_pct.clamp(0.0, MAX_REL_HUMIDITY_PCT), 1)
}

// =============================================================================
/// Calculates projected relative humidity $\varphi_{\text{proj}}(T_{\text{base}})$ (`%`) at base temp.
#[must_use]
pub fn calculate_projected_base_humidity(
    temp_in_c: f64,
    rel_humidity_in_pct: f64,
    temp_base_c: f64,
) -> f64 {
    let e_sat_in = calculate_saturation_vapor_pressure(temp_in_c);
    let e_sat_base = calculate_saturation_vapor_pressure(temp_base_c).max(MIN_VAPOR_PRESSURE_HPA);
    let raw_proj = rel_humidity_in_pct * (e_sat_in / e_sat_base);
    round_decimals(raw_proj.clamp(0.0, MAX_REL_HUMIDITY_PCT), 1)
}

// =============================================================================
/// Single timestamped indoor climate sample used for transient/window gradient detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateSample {
    /// Sample UNIX epoch timestamp in seconds.
    pub timestamp: f64,
    /// Indoor temperature in `°C`.
    pub temp_c: f64,
    /// Indoor relative humidity in `%`.
    pub rh_pct: f64,
}

// =============================================================================
/// Computed 15-minute humidity spike, 10-minute cooling drop, and exceedance timestamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateGradients {
    /// Maximum 15-minute relative humidity rise (`%`).
    pub delta_rh_15m: f64,
    /// Maximum 15-minute absolute humidity rise (`g/m³`).
    pub delta_x_15m: f64,
    /// Effective 10-minute temperature/vapor drop (`<= -1.0` indicates open window).
    pub min_delta_t_10m: f64,
    /// Earliest continuous exceedance timestamp above `humidity_threshold`.
    pub first_exceeded_ts: f64,
}

// =============================================================================
/// Computes 15-minute transient spikes and 10-minute open-window cooling/drying gradients.
#[must_use]
pub fn compute_climate_gradients(
    samples: &[ClimateSample],
    temp_in_c: f64,
    rel_humidity_in_pct: f64,
    humidity_threshold: f64,
    now_ts: f64,
) -> ClimateGradients {
    let mut ordered: Vec<ClimateSample> = if samples.is_empty() {
        vec![ClimateSample {
            timestamp: now_ts,
            temp_c: temp_in_c,
            rh_pct: rel_humidity_in_pct,
        }]
    } else {
        samples.to_vec()
    };
    ordered.sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp));
    let abs_now = calculate_absolute_humidity(temp_in_c, rel_humidity_in_pct);
    let (d_rh_15m, d_x_15m) = eval_15m_spikes(&ordered, rel_humidity_in_pct, abs_now, now_ts);
    let d_t_10m = eval_10m_window_drop(&ordered, temp_in_c, abs_now, now_ts);
    let first_exc = find_exceedance_start(&ordered, humidity_threshold);

    ClimateGradients {
        delta_rh_15m: round_decimals(d_rh_15m, 2),
        delta_x_15m: round_decimals(d_x_15m, 2),
        min_delta_t_10m: round_decimals(d_t_10m, 2),
        first_exceeded_ts: first_exc,
    }
}

// =============================================================================
/// Evaluates maximum 15-minute relative and absolute humidity jumps in `ordered`.
fn eval_15m_spikes(
    ordered: &[ClimateSample],
    rh_now: f64,
    abs_now: f64,
    now_ts: f64,
) -> (f64, f64) {
    let cutoff = now_ts - TRANSIENT_WINDOW_SECS;
    let mut any_in_win = false;
    let (mut max_drh, mut max_dx) = (0.0_f64, 0.0_f64);
    for s in ordered.iter().filter(|s| s.timestamp >= cutoff) {
        any_in_win = true;
        max_drh = max_drh.max(rh_now - s.rh_pct);
        max_dx = max_dx.max(abs_now - calculate_absolute_humidity(s.temp_c, s.rh_pct));
    }
    if !any_in_win {
        if let Some(first) = ordered.first() {
            max_drh = max_drh.max(rh_now - first.rh_pct);
            max_dx = max_dx.max(abs_now - calculate_absolute_humidity(first.temp_c, first.rh_pct));
        }
    }
    (max_drh, max_dx)
}

// =============================================================================
/// Evaluates 10-minute temperature/vapor drop with `+0.2 °C` rebound cancellation.
fn eval_10m_window_drop(
    ordered: &[ClimateSample],
    temp_now: f64,
    abs_now: f64,
    now_ts: f64,
) -> f64 {
    let cutoff = now_ts - WINDOW_DROP_WINDOW_SECS;
    let win: Vec<&ClimateSample> = ordered.iter().filter(|s| s.timestamp >= cutoff).collect();
    let slice: Vec<&ClimateSample> = if win.is_empty() {
        ordered.first().into_iter().collect()
    } else {
        win
    };
    let mut raw_dt = 0.0_f64;
    let mut raw_dx = 0.0_f64;
    let mut min_temp = temp_now;
    for s in slice {
        raw_dt = raw_dt.min(temp_now - s.temp_c);
        raw_dx = raw_dx.min(abs_now - calculate_absolute_humidity(s.temp_c, s.rh_pct));
        min_temp = min_temp.min(s.temp_c);
    }
    let is_rebounded = (temp_now - min_temp) >= (WINDOW_REBOUND_DELTA_C - FLOAT_EPSILON);
    if is_rebounded {
        0.0
    } else if raw_dx <= (WINDOW_DROP_ABS_GM3 + FLOAT_EPSILON) {
        raw_dt.min(raw_dx)
    } else {
        raw_dt
    }
}

// =============================================================================
/// Finds the first sample timestamp after the most recent non-exceeded reading.
fn find_exceedance_start(ordered: &[ClimateSample], humidity_threshold: f64) -> f64 {
    for idx in (0..ordered.len().saturating_sub(1)).rev() {
        if ordered[idx].rh_pct <= humidity_threshold {
            return ordered[idx + 1].timestamp;
        }
    }
    ordered.first().map_or(0.0, |s| s.timestamp)
}

// =============================================================================
/// Input parameters for [`calculate_guard_temperature`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardTempParams {
    /// Current indoor temperature (`°C`).
    pub temp_in_c: f64,
    /// Current indoor relative humidity (`%`).
    pub rel_humidity_in_pct: f64,
    /// Normal baseline/scheduled target temperature (`°C`).
    pub temp_base_c: f64,
    /// Optional outdoor air temperature (`°C`).
    pub temp_out_c: Option<f64>,
    /// Configured relative humidity trigger threshold (`%`).
    pub humidity_threshold: f64,
    /// Hysteresis band (`%`).
    pub hysteresis_percent: f64,
    /// Maximum allowed boost (`°C`) above `temp_base_c`.
    pub max_temp_boost_c: f64,
    /// Absolute comfort ceiling (`°C`).
    pub action_temp: f64,
}

impl Default for GuardTempParams {
    /// Returns standard `DIN 4108-2` default guard calculation parameters (`20 °C`, `60 %`).
    fn default() -> Self {
        Self {
            temp_in_c: 20.0,
            rel_humidity_in_pct: 60.0,
            temp_base_c: 20.0,
            temp_out_c: None,
            humidity_threshold: HUMIDITY_THRESHOLD,
            hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
            max_temp_boost_c: HUMIDITY_MAX_TEMP_BOOST_C,
            action_temp: HUMIDITY_ACTION_TEMP_CAP,
        }
    }
}

// =============================================================================
/// Calculates demand-exact `DIN 4108-2` guard target temperature $T_{\text{guard}}$ (`0.5 °C` step).
#[must_use]
pub fn calculate_guard_temperature(p: &GuardTempParams) -> f64 {
    let vp_in = calculate_vapor_pressure(p.temp_in_c, p.rel_humidity_in_pct);
    let target_rh = (p.humidity_threshold - p.hysteresis_percent / 2.0).max(MIN_TARGET_RH_PCT);
    let mut req = p.temp_base_c.max(calculate_temperature_from_vapor_pressure(
        vp_in * (100.0 / target_rh),
    ));

    if let Some(t_out) = p.temp_out_c {
        let t_si =
            calculate_temperature_from_vapor_pressure(vp_in * (100.0 / WALL_RH_TARGET_MAX_PCT));
        req = req
            .max(t_out + (t_si - t_out) / DIN_4108_FRSI)
            .max(t_out + THERMAL_BUOYANCY_DELTA_K);
    }
    let raw_cap = (p.temp_base_c + p.max_temp_boost_c).min(p.action_temp);
    let upper_cap = p
        .temp_base_c
        .max(((raw_cap + FLOAT_EPSILON) * HALF_DEGREE_FACTOR).floor() / HALF_DEGREE_FACTOR);
    let half_step = ((req * HALF_DEGREE_FACTOR) - FLOAT_EPSILON).ceil() / HALF_DEGREE_FACTOR;
    round_decimals(half_step.max(p.temp_base_c).min(upper_cap), 1)
}

// =============================================================================
/// Input parameters for [`resolve_ventilation_advice`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VentilationQuery {
    /// Current indoor relative humidity (`%`).
    pub rel_humidity_in_pct: f64,
    /// Configured trigger threshold (`%`).
    pub humidity_threshold: f64,
    /// Indoor absolute humidity $x_{\text{in}}$ (`g/m³`).
    pub abs_in_gm3: f64,
    /// Optional outdoor absolute humidity $x_{\text{out}}$ (`g/m³`).
    pub abs_out_gm3: Option<f64>,
    /// Whether `HumidityGuard` thermal shielding is currently active.
    pub guard_active: bool,
    /// Whether `HumidityControl` is enabled in this scope (`ISA-18.2` opt-in).
    pub rule_active: bool,
    /// Hysteresis band (`%`).
    pub hysteresis_percent: f64,
    /// Optional projected relative humidity at `T_base` (`%`).
    pub proj_base_rh_pct: Option<f64>,
}

// =============================================================================
/// Resolves outdoor-aware ventilation advice (`"none"`, `"ventilate_now"`, etc.).
#[must_use]
pub fn resolve_ventilation_advice(q: &VentilationQuery) -> &'static str {
    let rec_limit = q.humidity_threshold - q.hysteresis_percent;
    let is_exceeded = q.rel_humidity_in_pct > q.humidity_threshold
        || (q.guard_active && q.proj_base_rh_pct.is_none_or(|proj| proj > rec_limit));
    if !q.rule_active || !is_exceeded {
        return ADVICE_NONE;
    }
    let Some(abs_out) = q.abs_out_gm3 else {
        return ADVICE_VENTILATE_NOW;
    };
    let delta_x = q.abs_in_gm3 - abs_out;
    if delta_x >= (VENT_DELTA_NOW_GM3 - FLOAT_EPSILON) {
        ADVICE_VENTILATE_NOW
    } else if delta_x >= (VENT_DELTA_MARGINAL_GM3 - FLOAT_EPSILON) {
        ADVICE_VENTILATE_MARGINAL
    } else {
        ADVICE_DO_NOT_VENTILATE
    }
}

// =============================================================================
/// Computed 7-field `DIN 4108-2` psychrometric telemetry bundle for a Digital Twin device.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DevicePsychrometrics {
    /// Dew point temperature in `°C`.
    pub dew_point_c: Option<f64>,
    /// Absolute indoor humidity in `g/m³`.
    pub abs_humidity_gm3: Option<f64>,
    /// Critical thermal-bridge surface relative humidity (`%`, `f_Rsi = 0.70`).
    pub wall_surface_humidity_pct: Option<f64>,
    /// Projected relative humidity (`%`) when cooled to `temp_base_c`.
    pub projected_base_humidity_pct: Option<f64>,
    /// Active thermal-shielding flag.
    pub humidity_guard_active: bool,
    /// Outdoor-aware ventilation recommendation.
    pub ventilation_advice: Option<String>,
    /// Indoor-minus-outdoor absolute humidity delta (`g/m³`).
    pub humidity_delta_outdoor_gm3: Option<f64>,
}

// =============================================================================
/// Input parameters for [`compute_device_psychrometrics`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DevicePsychrometricsInput {
    /// Measured or propagated indoor temperature (`°C`).
    pub temp_in_c: Option<f64>,
    /// Measured or propagated indoor relative humidity (`%`).
    pub rel_humidity_in_pct: Option<f64>,
    /// Fallback thermostat target temperature (`°C`) when `temp_base_c` is unset.
    pub fallback_target_c: Option<f64>,
    /// Baseline target temperature (`°C`) for $\varphi_{\text{proj}}(T_{\text{base}})$.
    pub temp_base_c: Option<f64>,
    /// Optional outdoor temperature (`°C`).
    pub temp_out_c: Option<f64>,
    /// Optional outdoor absolute humidity (`g/m³`).
    pub abs_out_gm3: Option<f64>,
    /// Configured relative humidity trigger threshold (`%`).
    pub humidity_threshold: f64,
    /// Hysteresis band (`%`).
    pub hysteresis_percent: f64,
    /// Whether `HumidityGuard` thermal shielding is currently active.
    pub guard_active: bool,
    /// Whether `HumidityControl` is enabled in this scope.
    pub rule_active: bool,
}

// =============================================================================
/// Computes all 7 `DIN 4108-2` telemetry fields for a device given [`DevicePsychrometricsInput`].
#[must_use]
pub fn compute_device_psychrometrics(input: &DevicePsychrometricsInput) -> DevicePsychrometrics {
    let (Some(temp_in), Some(rh_in)) = (input.temp_in_c, input.rel_humidity_in_pct) else {
        return DevicePsychrometrics::default();
    };
    if rh_in <= 0.0 {
        return DevicePsychrometrics::default();
    }
    let fb_base = input
        .fallback_target_c
        .filter(|&t| t >= MIN_THERMOSTAT_TEMP)
        .unwrap_or(temp_in);
    let eff_base = input.temp_base_c.filter(|&t| t > 0.0).unwrap_or(fb_base);
    let abs_in = calculate_absolute_humidity(temp_in, rh_in);
    let proj_rh = calculate_projected_base_humidity(temp_in, rh_in, eff_base);
    let advice = resolve_ventilation_advice(&VentilationQuery {
        rel_humidity_in_pct: rh_in,
        humidity_threshold: input.humidity_threshold,
        abs_in_gm3: abs_in,
        abs_out_gm3: input.abs_out_gm3,
        guard_active: input.guard_active,
        rule_active: input.rule_active,
        hysteresis_percent: input.hysteresis_percent,
        proj_base_rh_pct: Some(proj_rh),
    });

    DevicePsychrometrics {
        dew_point_c: Some(calculate_dew_point(temp_in, rh_in)),
        abs_humidity_gm3: Some(abs_in),
        wall_surface_humidity_pct: input
            .temp_out_c
            .map(|t_out| calculate_wall_surface_humidity(temp_in, rh_in, t_out, DIN_4108_FRSI)),
        projected_base_humidity_pct: Some(proj_rh),
        humidity_guard_active: input.guard_active,
        ventilation_advice: Some(advice.to_owned()),
        humidity_delta_outdoor_gm3: input.abs_out_gm3.map(|out| round_decimals(abs_in - out, 2)),
    }
}
