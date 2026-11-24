//! Digital Twin device, software group, efficiency, & energy CLI formatters (`cowhome status`).
//!
//! Follows `_project-resources/docs/design_guidelines.md` (`ANSI/ISA-18.2` Calm UI & § 9.2):
//! renders aligned device tables via [`ConsolePrinter::print_table`], uses `20.8°C -> 21.0°C`
//! setpoint notation, highlights anomalies (`Offline`, `Low battery`, `Valve`), and hides
//! routine background noise such as nominal battery percentages (`> 20%`).

use std::collections::HashMap;

use serde_json::Value;

use crate::cli::output::ConsolePrinter;
use crate::core::constants::cli_style::{ANSI_CYAN, ANSI_GREEN, ANSI_RED, ANSI_YELLOW};
use crate::core::domain_constants::{
    analytics::energy_defaults::WH_PER_KWH,
    hardware::{device_capabilities, device_types, fritz_api::LOW_BATTERY_THRESHOLD},
};
use crate::core::psychrometrics::round_decimals;

/// Column headers used by [`ConsolePrinter::print_table`] for Smart Home device lists.
const DEVICE_TABLE_HEADERS: &[&str] = &["Device", "Type", "AIN", "Telemetry"];

// =============================================================================
/// Renders 24h Heating Efficiency, Smart Energy, Software Groups, & Smart Home Devices.
pub fn render_state_section(data: &Value, printer: ConsolePrinter) {
    render_heating_efficiency_line(data.get("heating_efficiency"), printer);
    render_energy_summary_line(data.get("energy_summary"), printer);
    printer.print_subsection("Smart Home Devices");
    let Some(devices) = data.get("devices").and_then(Value::as_array) else {
        eprintln!("  No devices found in Digital Twin.");
        return;
    };
    if devices.is_empty() {
        eprintln!("  No devices found in Digital Twin.");
        return;
    }

    let (group_order, group_members, ungrouped) = partition_devices_by_group(devices);
    for gid in group_order {
        if let Some(mems) = group_members.get(&gid) {
            render_device_group(&gid, mems, printer);
        }
    }
    if !ungrouped.is_empty() {
        eprintln!("  {}", printer.colorize("Standalone Devices:", ANSI_GREEN));
        render_device_table(&ungrouped, printer);
    }
}

// =============================================================================
/// Renders the house-wide 24h `Heating Efficiency` summary line when present.
fn render_heating_efficiency_line(eff_opt: Option<&Value>, printer: ConsolePrinter) {
    let Some(eff) = eff_opt.filter(|v| v.get("score").is_some()) else {
        return;
    };
    let pct = eff
        .get("score_percent")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let dem = eff
        .get("heating_demand_minutes")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let out_t = eff
        .get("avg_outdoor_temp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let sav = eff
        .get("estimated_savings_percent")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let rate_sfx = eff
        .get("heating_rate_c_per_hour")
        .and_then(Value::as_f64)
        .map_or_else(String::new, |r| format!("Rate +{r}°C/h · "));
    let badge = printer.colorize(&format!("{pct}%"), ANSI_GREEN);
    eprintln!(
        "\n  Heating Efficiency: {badge} · Demand {dem}m · {rate_sfx}Outdoor Ø {out_t}°C · Savings ~{sav}%"
    );
}

// =============================================================================
/// Renders the house-wide `Smart Energy` consumption & 30d forecast line.
fn render_energy_summary_line(en_opt: Option<&Value>, printer: ConsolePrinter) {
    let Some(en) = en_opt.filter(|v| v.get("today_kwh").is_some()) else {
        return;
    };
    let pw = en
        .get("power_current_w")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let tk = en.get("today_kwh").and_then(Value::as_f64).unwrap_or(0.0);
    let te = en
        .get("today_cost_eur")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mk = en.get("month_kwh").and_then(Value::as_f64).unwrap_or(0.0);
    let me = en
        .get("month_cost_eur")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let pm = en
        .get("projected_month_kwh")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let pe = en
        .get("projected_month_cost_eur")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let badge = printer.colorize(&format!("{pw} W"), ANSI_GREEN);
    eprintln!(
        "  Smart Energy:       {badge} · Today {tk} kWh ({te:.2} €) · 30d {mk} kWh ({me:.2} €) · Forecast {pm} kWh ({pe:.2} €)"
    );
}

// =============================================================================
/// Splits `/state` `devices` into ordered `(group_ids, group_members_map, ungrouped)`.
fn partition_devices_by_group(
    devices: &[Value],
) -> (Vec<String>, HashMap<String, Vec<&Value>>, Vec<&Value>) {
    let mut group_order: Vec<String> = Vec::new();
    let mut group_members: HashMap<String, Vec<&Value>> = HashMap::new();
    let mut ungrouped: Vec<&Value> = Vec::new();
    for dev in devices {
        let gid_opt = dev
            .get("group_id")
            .or_else(|| dev.get("group_ain"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(gid) = gid_opt {
            if !group_members.contains_key(gid) {
                group_order.push(gid.to_owned());
            }
            group_members.entry(gid.to_owned()).or_default().push(dev);
        } else {
            ungrouped.push(dev);
        }
    }
    (group_order, group_members, ungrouped)
}

// =============================================================================
/// Renders one Software Group header, active mode/guard badges, and member table.
fn render_device_group(gid: &str, members: &[&Value], printer: ConsolePrinter) {
    let first = members.first().copied();
    let gname = first
        .and_then(|m| m.get("managed_by").and_then(Value::as_str))
        .unwrap_or(gid);
    let target = first
        .and_then(|m| m.get("temperature_target").and_then(Value::as_f64))
        .unwrap_or(0.0);
    let badge_sfx = format_group_badges(members, target);
    let title = printer.colorize(&format!("Room Group: {gname}"), ANSI_GREEN);
    eprintln!("  {title} · {gid} · Target {target}°C{badge_sfx}");
    render_device_table(members, printer);
    eprintln!();
}

// =============================================================================
/// Builds `Pre-Heat` / `Guard active` badge suffix for a Room Group header.
fn format_group_badges(members: &[&Value], fallback_target: f64) -> String {
    let mut badges = Vec::new();
    if let Some(pre) = members
        .iter()
        .copied()
        .find(|m| m.get("preheat_active").and_then(Value::as_bool) == Some(true))
    {
        badges.push(format_preheat_badge(pre, fallback_target));
    } else if let Some(reason) = members
        .iter()
        .find_map(|m| m.get("preheat_paused_reason").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
    {
        badges.push(format!("Pre-Heat paused: {reason}"));
    }
    if members
        .iter()
        .any(|m| m.get("humidity_guard_active").and_then(Value::as_bool) == Some(true))
    {
        badges.push(String::from("Guard active"));
    }
    if badges.is_empty() {
        String::new()
    } else {
        format!(" · {}", badges.join(" · "))
    }
}

// =============================================================================
/// Formats `"Pre-Heat -> 21.0°C (06:30)"` from a thermostat JSON object.
fn format_preheat_badge(dev: &Value, fallback_target: f64) -> String {
    let tgt = dev
        .get("preheat_target_temp")
        .and_then(Value::as_f64)
        .unwrap_or(fallback_target);
    match dev
        .get("preheat_target_time")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(time_str) => format!("Pre-Heat -> {tgt}°C at {time_str}"),
        None => format!("Pre-Heat -> {tgt}°C"),
    }
}

// =============================================================================
/// Renders `devices` as a clean, ANSI-aware aligned table (`Device`, `Type`, `AIN`, `Telemetry`).
fn render_device_table(devices: &[&Value], printer: ConsolePrinter) {
    let rows: Vec<Vec<String>> = devices
        .iter()
        .map(|dev| {
            vec![
                printer.colorize(str_or_default(dev, "name", "Unknown"), ANSI_CYAN),
                str_or_default(dev, "device_type", "unknown").to_owned(),
                str_or_default(dev, "ain", "Unknown").to_owned(),
                format_device_metrics(dev, printer),
            ]
        })
        .collect();
    printer.print_table(DEVICE_TABLE_HEADERS, &rows, Some(&[16, 10, 14, 24]));
}

// =============================================================================
/// Dispatches Calm-UI telemetry formatting by `device_type`.
fn format_device_metrics(dev: &Value, printer: ConsolePrinter) -> String {
    match str_or_default(dev, "device_type", "") {
        t if t == device_types::THERMOSTAT => format_thermostat_metrics(dev, printer),
        t if t == device_types::SWITCH => format_switch_metrics(dev, printer),
        _ => format_sensor_or_router_metrics(dev, printer),
    }
}

// =============================================================================
/// Formats `thermostat` metrics (`20.8°C -> 21.0°C` + active modes & anomalies only).
fn format_thermostat_metrics(dev: &Value, printer: ConsolePrinter) -> String {
    let cur = dev
        .get("temperature_current")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let tgt = dev
        .get("temperature_target")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let tgt_str = if tgt <= 0.0 {
        String::from("OFF")
    } else {
        format!("{tgt}°C")
    };
    let mut parts = vec![format!("{cur}°C -> {tgt_str}")];
    if dev.get("present").and_then(Value::as_bool) == Some(false) {
        parts.push(printer.colorize("Offline", ANSI_RED));
    }
    if dev.get("preheat_active").and_then(Value::as_bool) == Some(true) {
        parts.push(format_preheat_badge(dev, tgt));
    } else if let Some(r) = dev
        .get("preheat_paused_reason")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Pre-Heat paused: {r}"));
    }
    if dev.get("humidity_guard_active").and_then(Value::as_bool) == Some(true) {
        parts.push(printer.colorize("Guard active", ANSI_YELLOW));
    }
    append_climate_and_anomaly_badges(dev, &mut parts, printer);
    parts.join(" · ")
}

// =============================================================================
/// Appends `Hum`, low-battery warning (`<= 20%` only), and valve fault badges onto `parts`.
fn append_climate_and_anomaly_badges(
    dev: &Value,
    parts: &mut Vec<String>,
    printer: ConsolePrinter,
) {
    if has_capability(dev, device_capabilities::HUMIDITY_SENSOR)
        && let Some(h) = dev.get("humidity").and_then(Value::as_f64)
    {
        parts.push(format!("{h}% RH"));
    }
    append_low_battery_if_needed(dev, parts, printer);
    if dev.get("error_code").and_then(Value::as_i64).unwrap_or(0) > 0 {
        let desc = str_or_default(dev, "error_description", "Valve fault");
        parts.push(printer.colorize(&format!("Valve: {desc}"), ANSI_RED));
    }
}

// =============================================================================
/// Formats `switch` or optical `power_meter` metrics (`✓ ON`/`✗ OFF`, `W`, `kWh`, `V`).
fn format_switch_metrics(dev: &Value, printer: ConsolePrinter) -> String {
    let present = dev.get("present").and_then(Value::as_bool).unwrap_or(true);
    let is_sw = has_capability(dev, device_capabilities::SWITCH)
        || dev
            .get("capabilities")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
    let state_txt = if !present {
        printer.status_badge(false, "ONLINE", "Offline")
    } else if is_sw {
        let is_on = dev.get("state").and_then(Value::as_bool) == Some(true);
        if is_on {
            printer.colorize("ON", ANSI_GREEN)
        } else {
            String::from("OFF")
        }
    } else {
        printer.status_badge(true, "Online", "Offline")
    };
    let mut parts = vec![state_txt];
    let pw = dev
        .get("power_current")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if pw != 0.0 || has_capability(dev, device_capabilities::POWER_METER) {
        parts.push(format!("{pw} W"));
    }
    let wh = dev
        .get("energy_total")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if wh > 0.0 {
        parts.push(format!("{} kWh", round_decimals(wh / WH_PER_KWH, 2)));
    }
    if let Some(v) = dev
        .get("voltage")
        .and_then(Value::as_f64)
        .filter(|&v| v > 0.0)
    {
        parts.push(format!("{v} V"));
    }
    append_low_battery_if_needed(dev, &mut parts, printer);
    parts.join(" · ")
}

// =============================================================================
/// Formats `sensor`, `repeater`, `router`, or generic device metrics (Calm UI).
fn format_sensor_or_router_metrics(dev: &Value, printer: ConsolePrinter) -> String {
    let present = dev.get("present").and_then(Value::as_bool) == Some(true);
    let mut parts = vec![printer.status_badge(present, "Online", "Offline")];
    if let Some(tc) = dev.get("temperature_current").and_then(Value::as_f64) {
        parts.push(format!("{tc}°C"));
    }
    if has_capability(dev, device_capabilities::HUMIDITY_SENSOR)
        && let Some(h) = dev.get("humidity").and_then(Value::as_f64)
    {
        parts.push(format!("{h}% RH"));
    }
    append_low_battery_if_needed(dev, &mut parts, printer);
    parts.join(" · ")
}

// =============================================================================
/// Emits `Low battery X%` only when `battery_low == true` or `battery_level <= 20%`.
fn append_low_battery_if_needed(dev: &Value, parts: &mut Vec<String>, printer: ConsolePrinter) {
    let low_flag = dev.get("battery_low").and_then(Value::as_bool) == Some(true);
    let bat_opt = dev.get("battery_level").and_then(Value::as_i64);
    match (low_flag, bat_opt) {
        (_, Some(pct)) if pct <= i64::from(LOW_BATTERY_THRESHOLD) => {
            parts.push(printer.colorize(&format!("Low battery {pct}%"), ANSI_YELLOW));
        }
        (true, Some(pct)) => {
            parts.push(printer.colorize(&format!("Low battery {pct}%"), ANSI_YELLOW));
        }
        (true, None) => parts.push(printer.colorize("Low battery", ANSI_YELLOW)),
        _ => {}
    }
}

// =============================================================================
/// Returns `true` if `dev.capabilities` contains `cap_name`.
fn has_capability(dev: &Value, cap_name: &str) -> bool {
    dev.get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|c| c.as_str() == Some(cap_name)))
}

// =============================================================================
/// Extracts a string slice field from `v`, defaulting to `fallback`.
pub(super) fn str_or_default<'a>(v: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or(fallback)
}
