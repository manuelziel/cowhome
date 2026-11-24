//! Schedule, seasonal window, and `VacationMode` target evaluation for 5-day HDD forecasts.
//!
//! Separated from [`super`] so slot parsing, weekday resolution, and guard overrides
//! remain focused (`< 350` lines) and reusable across forecast and automation layers.

use std::collections::HashMap;

use serde_json::Value;

use crate::core::domain_constants::{
    analytics::efficiency_defaults::{FROST_IDLE_MAX_TEMP, MINUTES_PER_DAY, SECONDS_PER_DAY},
    automation::{
        automation_defaults::{
            FROST_PROTECTION_TEMP, MINUTES_PER_DAY_U16, OUTDOOR_TEMP_THRESHOLD,
            SCHEDULE_SLOT_MINUTES, SUMMER_PAUSE_END_DATE, SUMMER_PAUSE_START_DATE,
            VACATION_PRE_HEAT_HOURS,
        },
        automation_identifiers::{
            HEATING_SCHEDULE, VACATION_MODE, WEATHER_SMART_HEATING, WEEKDAYS,
        },
    },
    hardware::special_devices::GROUP_ID_PREFIX,
};
use crate::core::psychrometrics::round_decimals;
use crate::core::time_util::normalize_ain;

use super::{DeviceForecastRequest, NormalizedDay, RuleSnapshot};

// =============================================================================
/// Parses an `"HH:MM"` wall-clock string into minutes since midnight (`0`..=`1440`).
#[must_use]
pub fn parse_hhmm_minutes(hhmm: &str) -> Option<u16> {
    let (h_str, m_str) = hhmm.trim().split_once(':')?;
    let hours: u16 = h_str.parse().ok()?;
    let minutes: u16 = m_str.parse().ok()?;
    if hours == 24 && minutes == 0 {
        return Some(MINUTES_PER_DAY_U16);
    }
    if hours < 24 && minutes < 60 {
        Some(hours * 60 + minutes)
    } else {
        None
    }
}

// =============================================================================
/// Maps English, German, or numeric (`0`..`6`) weekday tokens to canonical `"mon"`..`"sun"`.
#[must_use]
pub fn canonical_weekday_key(raw_key: &str) -> Option<&'static str> {
    match raw_key.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" | "mo" | "montag" | "0" => Some("mon"),
        "tue" | "tuesday" | "di" | "dienstag" | "1" => Some("tue"),
        "wed" | "wednesday" | "mi" | "mittwoch" | "2" => Some("wed"),
        "thu" | "thursday" | "do" | "donnerstag" | "3" => Some("thu"),
        "fri" | "friday" | "fr" | "freitag" | "4" => Some("fri"),
        "sat" | "saturday" | "sa" | "samstag" | "5" => Some("sat"),
        "sun" | "sunday" | "so" | "sonntag" | "6" => Some("sun"),
        _ => None,
    }
}

// =============================================================================
/// Checks whether an `"MM-DD"` date falls inside `[start_md, end_md]` (supports year wrap).
#[must_use]
pub fn is_date_within_window(month_day: &str, start_md: &str, end_md: &str) -> bool {
    let s = if start_md.trim().is_empty() {
        SUMMER_PAUSE_START_DATE
    } else {
        start_md.trim()
    };
    let e = if end_md.trim().is_empty() {
        SUMMER_PAUSE_END_DATE
    } else {
        end_md.trim()
    };
    if s <= e {
        month_day >= s && month_day <= e
    } else {
        month_day >= s || month_day <= e
    }
}

// =============================================================================
/// Resolves `(daily_target_temp, heating_limit_temp, is_paused_or_frost)` for `day`.
pub(super) fn resolve_day_target(
    day: &NormalizedDay,
    req: &DeviceForecastRequest<'_>,
) -> (f64, f64, bool) {
    let frost = req
        .global_vacation
        .and_then(|v| check_vacation_override(v, req.ain, req.group_id, day.day_ts))
        .or_else(|| {
            req.rules
                .get(VACATION_MODE)
                .and_then(|v| check_vacation_override(v, req.ain, req.group_id, day.day_ts))
        });
    if let Some(frost_temp) = frost {
        return (frost_temp, OUTDOOR_TEMP_THRESHOLD, true);
    }
    let (paused, limit_t) = check_summer_pause(
        req.rules.get(WEATHER_SMART_HEATING),
        &day.month_day,
        day.temp_max,
    );
    if paused {
        return (FROST_PROTECTION_TEMP, limit_t, true);
    }
    if let Some(sched) = req.rules.get(HEATING_SCHEDULE).filter(|s| s.is_active) {
        let days_val = sched.config.get("days");
        let (cur_slots, prev_slots) = resolve_weekday_slots(days_val, day.weekday);
        let tgt = compute_weekday_schedule_target(
            &cur_slots,
            req.comfort_temp,
            req.eco_temp,
            &prev_slots,
        );
        return (tgt, limit_t, tgt <= FROST_IDLE_MAX_TEMP);
    }
    let guard_on = req.global_vacation.is_some_and(|v| v.is_active)
        || req.rules.get(VACATION_MODE).is_some_and(|v| v.is_active)
        || req
            .rules
            .get(WEATHER_SMART_HEATING)
            .is_some_and(|w| w.is_active);
    let eff_cur = if guard_on && req.current_target <= FROST_IDLE_MAX_TEMP {
        req.comfort_temp
    } else {
        req.current_target
    };
    (eff_cur, limit_t, eff_cur <= FROST_IDLE_MAX_TEMP)
}

// =============================================================================
/// Evaluates whether `VacationMode` forces frost protection on `day_ts`.
fn check_vacation_override(
    vac: &RuleSnapshot,
    ain: &str,
    group_id: Option<&str>,
    day_ts: f64,
) -> Option<f64> {
    if !vac.is_active {
        return None;
    }
    let norm_ain = normalize_ain(ain);
    if json_str_list(&vac.config, "ignored_devices")
        .iter()
        .any(|d| normalize_ain(d) == norm_ain && !norm_ain.is_empty())
    {
        return None;
    }
    if let Some(gid) = group_id {
        let raw_gid = gid.trim_start_matches(GROUP_ID_PREFIX).trim();
        if json_str_list(&vac.config, "ignored_groups")
            .iter()
            .any(|g| g.trim_start_matches(GROUP_ID_PREFIX).trim() == raw_gid)
        {
            return None;
        }
    }
    let end_ts = vac
        .config
        .get("end_date_timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let pre_hrs = vac
        .config
        .get("pre_heat_hours")
        .and_then(Value::as_f64)
        .unwrap_or(VACATION_PRE_HEAT_HOURS as f64);
    let midday_ts = day_ts + (SECONDS_PER_DAY / 2.0);
    if end_ts > 0.0 && midday_ts >= (end_ts - pre_hrs * 3600.0) {
        return None;
    }
    let frost = vac
        .config
        .get("frost_protection_temp")
        .and_then(Value::as_f64)
        .filter(|&t| t > 0.0)
        .unwrap_or(FROST_PROTECTION_TEMP);
    Some(frost)
}

// =============================================================================
/// Evaluates `WeatherSmartHeating` (`Summer Pause`) on `month_day` for `temp_max`.
fn check_summer_pause(wea: Option<&RuleSnapshot>, month_day: &str, temp_max: f64) -> (bool, f64) {
    let Some(rule) = wea.filter(|r| r.is_active) else {
        return (false, OUTDOOR_TEMP_THRESHOLD);
    };
    let threshold = rule
        .config
        .get("outdoor_temp_threshold")
        .and_then(Value::as_f64)
        .filter(|&t| t > 0.0)
        .unwrap_or(OUTDOOR_TEMP_THRESHOLD);
    let start_md = rule
        .config
        .get("start_date")
        .and_then(Value::as_str)
        .unwrap_or(SUMMER_PAUSE_START_DATE);
    let end_md = rule
        .config
        .get("end_date")
        .and_then(Value::as_str)
        .unwrap_or(SUMMER_PAUSE_END_DATE);
    let in_season = is_date_within_window(month_day, start_md, end_md);
    let eff_limit = if in_season {
        threshold
    } else {
        OUTDOOR_TEMP_THRESHOLD
    };
    (in_season && temp_max >= threshold, eff_limit)
}

// =============================================================================
/// Extracts `(current_day_slots, previous_day_slots)` from a JSON `days` map.
fn resolve_weekday_slots(days_val: Option<&Value>, weekday_key: &str) -> (Vec<Value>, Vec<Value>) {
    let Some(obj) = days_val.and_then(Value::as_object) else {
        return (Vec::new(), Vec::new());
    };
    let mut normalized: HashMap<&'static str, Vec<Value>> = HashMap::new();
    for (k, v) in obj {
        if let (Some(canon), Some(arr)) = (canonical_weekday_key(k), v.as_array()) {
            normalized.insert(canon, arr.clone());
        }
    }
    let idx = WEEKDAYS.iter().position(|&w| w == weekday_key).unwrap_or(0);
    let prev_key = WEEKDAYS[(idx + 6) % 7];
    let cur = normalized.get(weekday_key).cloned().unwrap_or_default();
    let prev = normalized
        .get(prev_key)
        .cloned()
        .unwrap_or_else(|| cur.clone());
    (cur, prev)
}

// =============================================================================
/// Computes the 24h time-weighted average target temperature (`°C`) for one weekday.
fn compute_weekday_schedule_target(
    slots: &[Value],
    comfort_temp: f64,
    eco_temp: f64,
    prev_slots: &[Value],
) -> f64 {
    let mut intervals: Vec<(u16, u16, f64)> = Vec::new();
    for p_slot in prev_slots {
        if let Some((s, e, t)) = extract_slot_span(p_slot, comfort_temp)
            && s > e
            && e > 0
        {
            intervals.push((0, e, t));
        }
    }
    for slot in slots {
        if let Some((s, e, t)) = extract_slot_span(slot, comfort_temp) {
            intervals.push((s, if e > s { e } else { MINUTES_PER_DAY_U16 }, t));
        }
    }
    if intervals.is_empty() {
        return round_decimals(eco_temp, 2);
    }
    intervals.sort_by_key(|&(s, _, _)| s);
    let (mut cursor, mut comfort_mins, mut weighted_sum) = (0_u16, 0_u16, 0.0_f64);
    for (s, e, t) in intervals {
        let eff_start = s.max(cursor);
        let eff_end = e.min(MINUTES_PER_DAY_U16);
        if eff_end > eff_start {
            let dur = eff_end - eff_start;
            comfort_mins += dur;
            weighted_sum += f64::from(dur) * t;
            cursor = eff_end;
        }
    }
    let eco_mins = MINUTES_PER_DAY_U16.saturating_sub(comfort_mins);
    round_decimals(
        (weighted_sum + f64::from(eco_mins) * eco_temp) / MINUTES_PER_DAY,
        2,
    )
}

// =============================================================================
/// Parses `(start_minute, end_minute, target_temp)` from a flexible JSON schedule slot.
fn extract_slot_span(slot: &Value, comfort_temp: f64) -> Option<(u16, u16, f64)> {
    if let Some(raw_str) = slot.as_str() {
        let (s, e) = raw_str.split_once('-').unwrap_or((raw_str, ""));
        let start_m = parse_hhmm_minutes(s)?;
        let end_m = resolve_slot_end(start_m, e)?;
        return (start_m != end_m).then_some((start_m, end_m, comfort_temp));
    }
    let obj = slot.as_object()?;
    if is_slot_disabled(obj) {
        return None;
    }
    let s_raw = ["start", "start_time", "from", "time", "slot"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))?;
    let e_raw = ["end", "end_time", "to"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .unwrap_or("");
    let start_m = parse_hhmm_minutes(s_raw)?;
    let end_m = resolve_slot_end(start_m, e_raw)?;
    if start_m == end_m {
        return None;
    }
    let temp = ["temp", "target_temp", "temperature"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_f64))
        .filter(|&t| t > 0.0)
        .unwrap_or(comfort_temp);
    Some((start_m, end_m, temp))
}

// =============================================================================
/// Resolves `end_minute` (`0` and `1439` normalized to `1440`) for a slot starting at `start_m`.
fn resolve_slot_end(start_m: u16, end_raw: &str) -> Option<u16> {
    let raw_end = if end_raw.trim().is_empty() {
        (start_m + SCHEDULE_SLOT_MINUTES).min(MINUTES_PER_DAY_U16)
    } else {
        parse_hhmm_minutes(end_raw)?
    };
    Some(if raw_end == 0 || raw_end == (MINUTES_PER_DAY_U16 - 1) {
        MINUTES_PER_DAY_U16
    } else {
        raw_end
    })
}

// =============================================================================
/// Checks whether a JSON slot object carries an explicit `false` / `"off"` / `"eco"` flag.
fn is_slot_disabled(obj: &serde_json::Map<String, Value>) -> bool {
    for key in ["state", "on", "enabled", "active"] {
        match obj.get(key) {
            Some(Value::Bool(false)) => return true,
            Some(Value::String(s))
                if matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "off"
                ) =>
            {
                return true;
            }
            _ => {}
        }
    }
    for key in ["mode", "type", "preset"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str)
            && matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "off" | "eco" | "absenk" | "false" | "0"
            )
        {
            return true;
        }
    }
    false
}

// =============================================================================
/// Extracts a string list property from a JSON object cleanly.
fn json_str_list(obj: &Value, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
