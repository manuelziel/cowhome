//! Pure slot/block parsing and midnight-continuous phase ID helpers for `HeatingSchedule`.
//!
//! Separated from [`super::schedule`] so edge-triggered actuation and `HH:MM` block
//! coalescing each remain compact (`<= 300` lines).

use serde_json::Value;

use super::actuation::RULE_TEMP_EPSILON;
pub(super) use crate::core::domain_constants::automation::automation_defaults::MINUTES_PER_DAY_U16 as MINUTES_PER_DAY;
use crate::core::domain_constants::automation::{
    automation_defaults::{SCHEDULE_DEFAULT_COMFORT_TEMP, SCHEDULE_SLOT_MINUTES},
    automation_identifiers::WEEKDAYS,
};
use crate::core::heating_forecast::parse_hhmm_minutes;
use crate::models::automation::HeatingScheduleConfig;

// =============================================================================
/// Builds a midnight-continuous slot ID for an active comfort block.
#[must_use]
pub(super) fn resolve_comfort_slot_id(
    cfg: &HeatingScheduleConfig,
    day_idx: usize,
    start_m: u16,
    end_m: u16,
    temp: f64,
) -> String {
    let day_key = WEEKDAYS[day_idx];
    if start_m == 0 {
        let prev_idx = (day_idx + 6) % 7;
        let (prev_k, prev2_k) = (WEEKDAYS[prev_idx], WEEKDAYS[(prev_idx + 6) % 7]);
        for (ps, pe, pt) in parse_day_blocks(
            cfg.days.get(prev_k).map_or(&[], Vec::as_slice),
            cfg.comfort_temp,
            cfg.days.get(prev2_k).map(Vec::as_slice),
        ) {
            if pe == MINUTES_PER_DAY && (pt - temp).abs() < RULE_TEMP_EPSILON {
                return format!("{prev_k}:{ps}-{end_m}");
            }
        }
    }
    if end_m == MINUTES_PER_DAY {
        let next_k = WEEKDAYS[(day_idx + 1) % 7];
        for (ns, ne, nt) in parse_day_blocks(
            cfg.days.get(next_k).map_or(&[], Vec::as_slice),
            cfg.comfort_temp,
            cfg.days.get(day_key).map(Vec::as_slice),
        ) {
            if ns == 0 && (nt - temp).abs() < RULE_TEMP_EPSILON {
                return format!("{day_key}:{start_m}-{ne}");
            }
        }
    }
    format!("{day_key}:{start_m}-{end_m}")
}

// =============================================================================
/// Builds a midnight-continuous slot ID for an active eco setback phase.
#[must_use]
pub(super) fn resolve_eco_slot_id(
    cfg: &HeatingScheduleConfig,
    day_idx: usize,
    prev_end: u16,
) -> String {
    if prev_end > 0 {
        return format!("{}:eco:{prev_end}", WEEKDAYS[day_idx]);
    }
    for offset in 1..=7 {
        let look_idx = (day_idx + 7 - offset) % 7;
        let look_k = WEEKDAYS[look_idx];
        let prev_k = WEEKDAYS[(look_idx + 6) % 7];
        let blocks = parse_day_blocks(
            cfg.days.get(look_k).map_or(&[], Vec::as_slice),
            cfg.comfort_temp,
            cfg.days.get(prev_k).map(Vec::as_slice),
        );
        if let Some(last_end) = blocks.into_iter().map(|b| b.1).max() {
            return format!("{look_k}:eco:{last_end}");
        }
    }
    String::from("week:eco:0")
}

// =============================================================================
/// Parses, handles overnight spillover, and coalesces `(start_min, end_min, temp)` blocks.
#[must_use]
pub(super) fn parse_day_blocks(
    raw_blocks: &[Value],
    default_comfort: f64,
    prev_day_blocks: Option<&[Value]>,
) -> Vec<(u16, u16, f64)> {
    let mut parsed = Vec::new();
    let src_prev = prev_day_blocks.unwrap_or(raw_blocks);
    for prev_item in src_prev {
        if let Some((s, e, t)) = extract_block_span(prev_item, default_comfort) {
            if s > e && e > 0 {
                parsed.push((0, e, t));
            }
        }
    }
    for item in raw_blocks {
        let Some((s, e, t)) = extract_block_span(item, default_comfort) else {
            continue;
        };
        if s < e && e <= MINUTES_PER_DAY {
            parsed.push((s, e, t));
        } else if e < s && s < MINUTES_PER_DAY {
            parsed.push((s, MINUTES_PER_DAY, t));
        }
    }
    parsed.sort_by_key(|&(s, _, _)| s);
    let mut merged: Vec<(u16, u16, f64)> = Vec::with_capacity(parsed.len());
    for (s, e, t) in parsed {
        if let Some(last) = merged.last_mut() {
            if last.1 >= s && (last.2 - t).abs() < RULE_TEMP_EPSILON {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e, t));
    }
    merged
}

// =============================================================================
/// Extracts `(start_minute, end_minute, resolved_temp)` from a slot JSON object or string.
fn extract_block_span(item: &Value, default_comfort: f64) -> Option<(u16, u16, f64)> {
    if let Some(s_str) = item.as_str() {
        let (s, e) = s_str.split_once('-').unwrap_or((s_str, ""));
        let start_m = parse_hhmm_minutes(s)?;
        if start_m >= MINUTES_PER_DAY {
            return None;
        }
        let end_m = resolve_end_minute(start_m, e)?;
        let fb = if default_comfort > 0.0 {
            default_comfort
        } else {
            SCHEDULE_DEFAULT_COMFORT_TEMP
        };
        return (start_m != end_m).then_some((start_m, end_m, fb));
    }
    let obj = item.as_object()?;
    let start_raw = ["start", "start_time", "from", "time", "slot"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))?;
    let end_raw = ["end", "end_time", "to"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .unwrap_or("");
    let start_m = parse_hhmm_minutes(start_raw)?;
    if start_m >= MINUTES_PER_DAY {
        return None;
    }
    let end_m = resolve_end_minute(start_m, end_raw)?;
    (start_m != end_m && end_m > 0 && end_m <= MINUTES_PER_DAY)
        .then(|| (start_m, end_m, resolve_block_temp(obj, default_comfort)))
}

// =============================================================================
/// Resolves `end_minute` (`0` and `1439` normalized to `1440`).
fn resolve_end_minute(start_m: u16, end_raw: &str) -> Option<u16> {
    let raw = if end_raw.trim().is_empty() {
        (start_m + SCHEDULE_SLOT_MINUTES).min(MINUTES_PER_DAY)
    } else {
        parse_hhmm_minutes(end_raw)?
    };
    Some(if raw == 0 || raw == MINUTES_PER_DAY - 1 {
        MINUTES_PER_DAY
    } else {
        raw
    })
}

// =============================================================================
/// Resolves block target value (`0.0` when explicitly OFF/eco, `> 0.0` otherwise).
fn resolve_block_temp(obj: &serde_json::Map<String, Value>, default_comfort: f64) -> f64 {
    let fb = if default_comfort > 0.0 {
        default_comfort
    } else {
        SCHEDULE_DEFAULT_COMFORT_TEMP
    };
    let active_temp = ["temp", "target_temp", "temperature"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_f64))
        .filter(|&t| t > 0.0)
        .unwrap_or(fb);
    for key in [
        "state", "on", "enabled", "active", "is_on", "power", "switch",
    ] {
        if let Some(val) = obj.get(key).filter(|v| !v.is_null()) {
            let is_on = match val {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
                Value::String(s) => {
                    matches!(
                        s.trim().to_ascii_lowercase().as_str(),
                        "true" | "1" | "on" | "yes"
                    )
                }
                _ => false,
            };
            return if is_on { active_temp } else { 0.0 };
        }
    }
    let mode = ["mode", "type", "preset"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(mode.as_str(), "off" | "eco" | "absenk" | "false" | "0") {
        0.0
    } else {
        active_temp
    }
}
