//! Validated prompt primitives for interactive configuration editing (`cowhome config`).
//!
//! Enforces `clig.dev` & Calm-UI ergonomics: concise prompt labels with bracketed defaults,
//! in-place validation retries on typos (preserving earlier inputs), and clean `EOF` exit.

use std::str::FromStr;

use crate::cli::input::{prompt_confirm, prompt_line};
use crate::cli::output::ConsolePrinter;
use crate::core::constants::service_defaults::MIN_API_KEY_LEN;
use crate::core::domain_constants::automation::weather_defaults::MAX_FORECAST_DAYS;
use crate::core::error::{AppError, AppResult};
use crate::core::validation::{
    is_api_key_secure, mask_secret, parse_ip_or_cidr, validate_coordinates, validate_host_or_ip,
    validate_http_endpoint, validate_safe_path, validate_timezone,
};

// =============================================================================
/// Wraps [`prompt_line`], mapping [`AppError::Cancelled`] cleanly to `Ok(None)`.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_or_cancel(label: &str, default: Option<&str>) -> AppResult<Option<String>> {
    match prompt_line(label, default) {
        Ok(val) => Ok(Some(val)),
        Err(AppError::Cancelled) => Ok(None),
        Err(err) => Err(err),
    }
}

// =============================================================================
/// Wraps [`prompt_confirm`], mapping [`AppError::Cancelled`] cleanly to `Ok(None)`.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_bool_or_cancel(question: &str, default_yes: bool) -> AppResult<Option<bool>> {
    match prompt_confirm(question, default_yes) {
        Ok(val) => Ok(Some(val)),
        Err(AppError::Cancelled) => Ok(None),
        Err(err) => Err(err),
    }
}

// =============================================================================
/// Prompts for a hostname/IP until valid (`RFC 1035` / `IPv4` / `IPv6`) or cancelled.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_checked_host(
    label: &str,
    default: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<String>> {
    loop {
        let Some(host) = prompt_or_cancel(label, Some(default))? else {
            return Ok(None);
        };
        match validate_host_or_ip(&host) {
            Ok(()) => return Ok(Some(host)),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
}

// =============================================================================
/// Prompts for an `http://` or `https://` base URL until valid or cancelled.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_checked_url(
    label: &str,
    default: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<String>> {
    loop {
        let Some(url) = prompt_or_cancel(label, Some(default))? else {
            return Ok(None);
        };
        match validate_http_endpoint(&url) {
            Ok(()) => return Ok(Some(url)),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
}

// =============================================================================
/// Prompts for a safe filesystem path (no `..` traversal) until valid or cancelled.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_checked_path(
    label: &str,
    default: &str,
    field_name: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<String>> {
    loop {
        let Some(path) = prompt_or_cancel(label, Some(default))? else {
            return Ok(None);
        };
        match validate_safe_path(&path, field_name) {
            Ok(()) => return Ok(Some(path)),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
}

// =============================================================================
/// Prompts for a masked secret, keeping `current` when Enter is pressed empty.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_masked_secret(label: &str, current: &str) -> AppResult<Option<String>> {
    let hint = format!("{label} [{}, Enter to keep]", mask_secret(current));
    let Some(entered) = prompt_or_cancel(&hint, None)? else {
        return Ok(None);
    };
    if entered.is_empty() {
        Ok(Some(current.to_owned()))
    } else {
        Ok(Some(entered))
    }
}

// =============================================================================
/// Prompts for a strong API key (`>= 16` printable ASCII chars) until valid or cancelled.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_api_key(current_key: &str, printer: ConsolePrinter) -> AppResult<Option<String>> {
    loop {
        let Some(candidate) = prompt_masked_secret("API key", current_key)? else {
            return Ok(None);
        };
        if is_api_key_secure(&candidate) {
            return Ok(Some(candidate));
        }
        printer.print_error(&format!(
            "API key must be at least {MIN_API_KEY_LEN} printable ASCII characters (no placeholders)."
        ));
    }
}

// =============================================================================
/// Prompts for comma-separated `trusted_proxies` IPs/CIDRs until valid or cancelled.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_trusted_proxies(
    current: &[String],
    printer: ConsolePrinter,
) -> AppResult<Option<Vec<String>>> {
    let joined = current.join(", ");
    loop {
        let Some(raw) = prompt_or_cancel("Trusted proxies", Some(&joined))? else {
            return Ok(None);
        };
        let tokens: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(err) = tokens.iter().find_map(|t| parse_ip_or_cidr(t).err()) {
            printer.print_error(&err.to_string());
            continue;
        }
        return Ok(Some(tokens));
    }
}

// =============================================================================
/// Prompts and validates WGS-84 `(latitude, longitude)` coordinates with retry.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_lat_lon(
    cur_lat: f64,
    cur_lon: f64,
    printer: ConsolePrinter,
) -> AppResult<Option<(f64, f64)>> {
    let (lat_s, lon_s) = (cur_lat.to_string(), cur_lon.to_string());
    loop {
        let Some(raw_lat) = prompt_or_cancel("Latitude", Some(&lat_s))? else {
            return Ok(None);
        };
        let Some(raw_lon) = prompt_or_cancel("Longitude", Some(&lon_s))? else {
            return Ok(None);
        };
        let (Ok(lat), Ok(lon)) = (raw_lat.parse::<f64>(), raw_lon.parse::<f64>()) else {
            printer.print_error("Enter valid decimal numbers for latitude and longitude.");
            continue;
        };
        match validate_coordinates(lat, lon) {
            Ok(()) => return Ok(Some((lat, lon))),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
}

// =============================================================================
/// Prompts and validates an IANA timezone or `"auto"` with retry.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_checked_timezone(
    current: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<String>> {
    loop {
        let Some(tz) = prompt_or_cancel("Timezone", Some(current))? else {
            return Ok(None);
        };
        match validate_timezone(&tz) {
            Ok(()) => return Ok(Some(tz)),
            Err(err) => printer.print_error(&err.to_string()),
        }
    }
}

// =============================================================================
/// Prompts for `weather.update_interval` (`>= 1`) and `weather.forecast_days` (`1..=16`).
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_weather_horizon(
    cur_upd: u64,
    cur_days: u8,
    printer: ConsolePrinter,
) -> AppResult<Option<(u64, u8)>> {
    let Some(upd) = prompt_positive_int("Weather refresh in seconds", cur_upd, printer)? else {
        return Ok(None);
    };
    let days_s = cur_days.to_string();
    loop {
        let Some(raw_days) = prompt_or_cancel("Forecast days", Some(&days_s))? else {
            return Ok(None);
        };
        match raw_days.parse::<u8>() {
            Ok(days) if (1..=MAX_FORECAST_DAYS).contains(&days) => return Ok(Some((upd, days))),
            _ => printer.print_error(&format!(
                "Forecast days must be between 1 and {MAX_FORECAST_DAYS}."
            )),
        }
    }
}

// =============================================================================
/// Prompts for a strictly positive unsigned integer (`>= 1`) of type `T` with retry.
///
/// # Errors
/// Returns [`AppError::Io`] on unrecoverable terminal stream errors.
pub fn prompt_positive_int<T>(
    label: &str,
    default: T,
    printer: ConsolePrinter,
) -> AppResult<Option<T>>
where
    T: Copy + DisplayAndParse + PartialOrd + From<u8>,
{
    let def_str = default.to_display_string();
    loop {
        let Some(raw) = prompt_or_cancel(label, Some(&def_str))? else {
            return Ok(None);
        };
        match T::from_str(&raw) {
            Ok(val) if val >= T::from(1u8) => return Ok(Some(val)),
            _ => printer.print_error("Enter a positive whole number (>= 1)."),
        }
    }
}

// =============================================================================
/// Helper trait combining `ToString` and `FromStr` for generic integer prompts.
pub trait DisplayAndParse: FromStr {
    /// Formats `self` as a decimal default string.
    fn to_display_string(self) -> String;
}

impl DisplayAndParse for u16 {
    /// Formats `u16` as a decimal string.
    fn to_display_string(self) -> String {
        self.to_string()
    }
}

impl DisplayAndParse for u32 {
    /// Formats `u32` as a decimal string.
    fn to_display_string(self) -> String {
        self.to_string()
    }
}

impl DisplayAndParse for u64 {
    /// Formats `u64` as a decimal string.
    fn to_display_string(self) -> String {
        self.to_string()
    }
}
