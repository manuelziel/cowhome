//! `system_settings` key-value persistence & thermostat static calibration offsets.
//!
//! Provides LWW-protected zero-write upserts for bounded numeric system settings (tariffs,
//! base fees) and CowHome's authoritative thermostat calibration offsets (`-5.0..=+5.0 °C`).

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use super::Database;
use crate::core::domain_constants::hardware::fritz_api::{
    DEFAULT_THERMOSTAT_OFFSET, HALF_DEGREE_FACTOR, MAX_THERMOSTAT_OFFSET, MIN_THERMOSTAT_OFFSET,
    OFFSET_SETTING_KEY_PREFIX,
};
use crate::core::error::{AppError, AppResult};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::{build_setting_lww_key, normalize_ain, now_epoch_f64};

/// Floating-point tolerance when verifying `0.5 °C` quantization steps.
const OFFSET_STEP_EPSILON: f64 = 1e-4;

// =============================================================================
/// Validation specification for bounded numeric values stored in `system_settings`.
#[derive(Debug, Clone, Copy)]
pub(super) struct NumericSettingSpec<'a> {
    /// Setting key in `system_settings`.
    pub key: &'a str,
    /// Fallback default when missing or out of bounds.
    pub default: f64,
    /// Inclusive lower bound.
    pub min_val: f64,
    /// Inclusive upper bound.
    pub max_val: f64,
    /// Decimal places for rounding.
    pub decimals: i32,
}

impl Database {
    /// Reads the persisted CowHome static calibration offset (`°C`) for thermostat `ain`.
    #[must_use]
    pub fn get_thermostat_offset(&self, ain: &str) -> f64 {
        let key = format!("{OFFSET_SETTING_KEY_PREFIX}{}", normalize_ain(ain));
        let raw = self.get_numeric_setting(
            &key,
            DEFAULT_THERMOSTAT_OFFSET,
            MIN_THERMOSTAT_OFFSET,
            MAX_THERMOSTAT_OFFSET,
            1,
        );
        (raw * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR
    }

    /// Validates and persists a thermostat calibration offset (`-5.0..=+5.0 °C`, `0.5 °C` steps).
    ///
    /// Returns `(authoritative_offset_c, was_applied)`.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid offset or [`AppError::Database`] on SQL error.
    pub fn set_thermostat_offset(
        &self,
        ain: &str,
        raw_offset: &Value,
        effective_ts: Option<f64>,
    ) -> AppResult<(f64, bool)> {
        let val = validate_thermostat_offset(raw_offset)?;
        let setting_key = format!("{OFFSET_SETTING_KEY_PREFIX}{}", normalize_ain(ain));
        let eff_ts = effective_ts.unwrap_or_else(now_epoch_f64);
        let lww_key = build_setting_lww_key(&setting_key);
        if self.is_lww_stale(std::slice::from_ref(&lww_key), eff_ts) {
            return Ok((self.get_thermostat_offset(ain), false));
        }
        let formatted = format!("{val:.1}");
        self.upsert_system_setting(&setting_key, &formatted, eff_ts)?;
        self.record_lww_mutation(&[lww_key], eff_ts);
        Ok((val, true))
    }

    /// Reads and bounds-checks a floating-point setting from `system_settings`.
    #[must_use]
    pub(super) fn get_numeric_setting(
        &self,
        key: &str,
        default: f64,
        min_val: f64,
        max_val: f64,
        decimals: i32,
    ) -> f64 {
        let fetched = self.with_conn(|conn| {
            conn.query_row(
                "SELECT setting_value FROM system_settings WHERE setting_key = ? LIMIT 1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(AppError::from)
        });
        if let Ok(Some(text)) = fetched
            && let Ok(val) = text.trim().parse::<f64>()
            && val.is_finite()
            && (min_val..=max_val).contains(&val)
        {
            return round_decimals(val, decimals);
        }
        default
    }

    /// Validates and persists a bounded numeric `system_settings` entry with LWW protection.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid input or [`AppError::Database`] on SQL error.
    pub(super) fn set_numeric_setting(
        &self,
        spec: NumericSettingSpec<'_>,
        raw_val: &Value,
        effective_ts: Option<f64>,
    ) -> AppResult<(f64, bool)> {
        let parsed = parse_strict_f64(raw_val, spec.key)?;
        let val = round_decimals(parsed, spec.decimals);
        if !(spec.min_val..=spec.max_val).contains(&val) {
            return Err(AppError::Validation(format!(
                "{} must be between {} and {}",
                spec.key, spec.min_val, spec.max_val
            )));
        }
        let eff_ts = effective_ts.unwrap_or_else(now_epoch_f64);
        let lww_key = build_setting_lww_key(spec.key);
        if self.is_lww_stale(std::slice::from_ref(&lww_key), eff_ts) {
            let cur = self.get_numeric_setting(
                spec.key,
                spec.default,
                spec.min_val,
                spec.max_val,
                spec.decimals,
            );
            return Ok((cur, false));
        }
        self.upsert_system_setting(spec.key, &val.to_string(), eff_ts)?;
        self.record_lww_mutation(&[lww_key], eff_ts);
        Ok((val, true))
    }

    /// Performs a zero-write upsert into `system_settings` only when `setting_value` changes.
    fn upsert_system_setting(&self, key: &str, value: &str, eff_ts: f64) -> AppResult<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO system_settings (setting_key, setting_value, updated_at)
                 VALUES (?, ?, datetime(?, 'unixepoch'))
                 ON CONFLICT(setting_key) DO UPDATE
                 SET setting_value = excluded.setting_value, updated_at = excluded.updated_at
                 WHERE system_settings.setting_value IS NOT excluded.setting_value",
                params![key, value, eff_ts],
            )?;
            Ok(())
        })
    }
}

// =============================================================================
/// Parses a JSON scalar into `f64`, explicitly rejecting booleans and `null`.
fn parse_strict_f64(raw: &Value, field_name: &str) -> AppResult<f64> {
    let val = match raw {
        Value::Number(num) => num
            .as_f64()
            .ok_or_else(|| AppError::Validation(format!("{field_name} must be a valid number")))?,
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| AppError::Validation(format!("{field_name} must be a valid number")))?,
        _ => {
            return Err(AppError::Validation(format!(
                "{field_name} must be a number"
            )));
        }
    };
    if !val.is_finite() {
        return Err(AppError::Validation(format!(
            "{field_name} must be a finite number"
        )));
    }
    Ok(val)
}

// =============================================================================
/// Validates that a thermostat offset lies in `[-5.0, +5.0] °C` in `0.5 °C` steps.
fn validate_thermostat_offset(raw: &Value) -> AppResult<f64> {
    let val = parse_strict_f64(raw, "offset")?;
    if !(MIN_THERMOSTAT_OFFSET..=MAX_THERMOSTAT_OFFSET).contains(&val) {
        return Err(AppError::Validation(format!(
            "offset must be between {MIN_THERMOSTAT_OFFSET} and {MAX_THERMOSTAT_OFFSET} °C"
        )));
    }
    let rounded = (val * HALF_DEGREE_FACTOR).round() / HALF_DEGREE_FACTOR;
    if (val - rounded).abs() > OFFSET_STEP_EPSILON {
        return Err(AppError::Validation(
            "offset must be in 0.5 °C increments".into(),
        ));
    }
    Ok(rounded)
}
