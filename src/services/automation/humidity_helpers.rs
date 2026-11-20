//! Scope climate resolution, psychrometric propagation & trace helpers for `HumidityControl` v2.
//!
//! Separated from [`super::humidity`] so the 4-state `HumidityGuard` FSM and pure
//! climate/telemetry/step-down helpers each stay concise (`<= 450` lines).

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::actuation::humidity_guard_states;
use crate::core::domain_constants::{
    analytics::history_defaults::FLOAT_EPSILON,
    automation::{
        automation_defaults::{
            HUMIDITY_ACTION_TEMP_CAP, HUMIDITY_DURATION_MINUTES, HUMIDITY_HYSTERESIS_PERCENT,
            HUMIDITY_STEP_DOWN_DELTA_C as STEP_DOWN_DELTA_C,
            HUMIDITY_STEP_DOWN_INTERVAL_SECS as STEP_DOWN_INTERVAL_SECS, HUMIDITY_SUSTAIN_MINUTES,
            HUMIDITY_WINDOW_DROP_DELTA_C as WINDOW_DROP_DELTA_C, SCHEDULE_DEFAULT_COMFORT_TEMP,
        },
        psychrometrics::{DIN_4108_FRSI, WALL_RH_STEPDOWN_SAFE_PCT},
    },
    hardware::fritz_api::MIN_THERMOSTAT_TEMP,
};
use crate::core::psychrometrics::{
    calculate_guard_temperature, calculate_projected_base_humidity,
    calculate_wall_surface_humidity, compute_device_psychrometrics, ClimateSample,
    DevicePsychrometricsInput, GuardTempParams,
};
use crate::core::time_util::normalize_ain;
use crate::db::Database;
use crate::models::automation::HumidityControlConfig;
use crate::models::device::DeviceState;
use crate::models::registry::StateRegistry;
use crate::services::weather::WeatherService;

/// Tuple `(temp_in_c, rh_in_pct, temp_out_c, abs_out_gm3)`.
pub(super) type ScopeClimate = (f64, f64, Option<f64>, Option<f64>);

// =============================================================================
/// Resolves `(source_ain, temp_in_c, rh_in_pct)` across sensor and scope thermostats.
#[must_use]
pub(super) fn resolve_scope_climate(
    config: &HumidityControlConfig,
    registry: &StateRegistry,
) -> (String, Option<f64>, Option<f64>) {
    let actor_temps: Vec<f64> = config
        .actor_ain
        .iter()
        .filter_map(|a| registry.get_device(a))
        .filter_map(|d| d.as_thermostat().map(|t| t.temperature_current))
        .filter(|&t| t > 0.0)
        .collect();
    let fb_temp = (!actor_temps.is_empty())
        .then(|| actor_temps.iter().sum::<f64>() / (actor_temps.len() as f64));

    if !config.sensor_ain.trim().is_empty() {
        if let Some(s) = registry.get_device(&config.sensor_ain) {
            if let Some(rh) = s.humidity {
                let st = s
                    .as_sensor()
                    .and_then(|sf| sf.temperature_current)
                    .or_else(|| s.as_thermostat().map(|t| t.temperature_current))
                    .filter(|&v| v > 0.0)
                    .or(fb_temp);
                return (s.ain, st, Some(rh));
            }
        }
    }
    let cands: Vec<DeviceState> = config
        .actor_ain
        .iter()
        .filter_map(|a| registry.get_device(a))
        .filter(|d| d.humidity.is_some())
        .collect();
    let Some(prim) = cands.into_iter().max_by(|a, b| {
        a.humidity
            .unwrap_or(0.0)
            .total_cmp(&b.humidity.unwrap_or(0.0))
    }) else {
        return (String::new(), fb_temp, None);
    };
    let pt = prim
        .as_thermostat()
        .map(|t| t.temperature_current)
        .filter(|&v| v > 0.0)
        .or(fb_temp);
    (prim.ain, pt, prim.humidity)
}

// =============================================================================
/// Propagates `DIN 4108-2` psychrometrics onto both `sensor_ain` and all `actor_ain` devices.
pub(super) fn enrich_scope_psychrometrics(
    config: &HumidityControlConfig,
    registry: &StateRegistry,
    climate: ScopeClimate,
    base_temp: f64,
    guard_active: bool,
    rh_override: Option<f64>,
    rule_active: bool,
) {
    let mut targets = config.actor_ain.clone();
    if !config.sensor_ain.trim().is_empty() {
        targets.push(config.sensor_ain.clone());
    }
    let scope_rh = rh_override.or_else(|| (climate.1 > 0.0).then_some(climate.1));
    for ain in targets {
        let Some(dev) = registry.get_device(&ain) else {
            continue;
        };
        let Some(rh) = dev.humidity.or(scope_rh).filter(|&v| v > 0.0) else {
            continue;
        };
        let dev_temp = dev
            .as_thermostat()
            .map(|t| t.temperature_current)
            .or_else(|| dev.as_sensor().and_then(|s| s.temperature_current))
            .filter(|&t| t > 0.0 && dev.humidity.is_some())
            .or_else(|| (climate.0 > 0.0).then_some(climate.0));
        let psych = compute_device_psychrometrics(&DevicePsychrometricsInput {
            temp_in_c: dev_temp,
            rel_humidity_in_pct: Some(rh),
            fallback_target_c: dev.as_thermostat().map(|t| t.temperature_target),
            temp_base_c: Some(base_temp),
            temp_out_c: climate.2,
            abs_out_gm3: climate.3,
            humidity_threshold: config.humidity_threshold,
            hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
            guard_active: guard_active && normalize_ain(&ain) != normalize_ain(&config.sensor_ain),
            rule_active,
        });
        registry.mutate_device(&ain, |d| {
            if d.humidity.is_none() {
                d.humidity = Some(rh);
            }
            d.apply_psychrometrics(psych);
        });
    }
}

// =============================================================================
/// Extracts `(temp_out_c, abs_out_gm3)` from `WeatherService`.
#[must_use]
pub(super) fn outdoor_climate(
    weather_service: Option<&WeatherService>,
) -> (Option<f64>, Option<f64>) {
    weather_service
        .and_then(WeatherService::get_current_weather)
        .map_or((None, None), |cw| cw.outdoor_psychrometrics())
}

// =============================================================================
/// Builds a deterministic scope state key from `actor_ain` or `sensor_ain`.
#[must_use]
pub(super) fn build_scope_key(config: &HumidityControlConfig) -> String {
    let mut norms: Vec<String> = config
        .actor_ain
        .iter()
        .map(|a| normalize_ain(a))
        .filter(|s| !s.is_empty())
        .collect();
    norms.sort();
    if !norms.is_empty() {
        norms.join(",")
    } else {
        let s = normalize_ain(&config.sensor_ain);
        if s.is_empty() {
            String::from("global")
        } else {
            s
        }
    }
}

// =============================================================================
/// Returns `true` when any actor reports `window_open` or a `10 min` drop `<= -1.0`.
#[must_use]
pub(super) fn is_window_or_draft_active(
    config: &HumidityControlConfig,
    registry: &StateRegistry,
    d_t_10m: f64,
) -> bool {
    if d_t_10m <= WINDOW_DROP_DELTA_C + FLOAT_EPSILON {
        return true;
    }
    config.actor_ain.iter().any(|a| {
        registry
            .get_device(a)
            .and_then(|d| d.as_thermostat().map(|t| t.window_open))
            .unwrap_or(false)
    })
}

// =============================================================================
/// Resolves unboosted baseline temperature $T_{\text{base}}$ for `config`.
#[must_use]
pub(super) fn resolve_scope_base_temp(
    config: &HumidityControlConfig,
    registry: &StateRegistry,
    previous_targets: &HashMap<String, f64>,
    scheduled_base: Option<f64>,
) -> f64 {
    if let Some(sb) = scheduled_base.filter(|&t| t >= MIN_THERMOSTAT_TEMP) {
        return sb;
    }
    for ain in &config.actor_ain {
        if let Some(dev) = registry.get_device(ain) {
            if let Some(&saved) = previous_targets.get(&dev.ain) {
                return saved;
            }
        }
    }
    for ain in &config.actor_ain {
        if let Some(t) = registry
            .get_device(ain)
            .and_then(|d| d.as_thermostat().map(|t| t.temperature_target))
            .filter(|&t| t >= MIN_THERMOSTAT_TEMP)
        {
            return t;
        }
    }
    SCHEDULE_DEFAULT_COMFORT_TEMP
}

// =============================================================================
/// Resolves `THERMAL_SHIELDING` vs `30 min` `-0.5 °C` `RECOVERY_CHECK` step-down.
#[must_use]
pub(super) fn compute_stepdown_target(
    config: &HumidityControlConfig,
    registry: &StateRegistry,
    shielding_since: &HashMap<String, f64>,
    climate: ScopeClimate,
    base_temp: f64,
    t_guard: f64,
    now_ts: f64,
) -> (f64, &'static str) {
    let start_ts = config
        .actor_ain
        .iter()
        .filter_map(|a| registry.get_device(a))
        .filter_map(|d| shielding_since.get(&d.ain).copied())
        .reduce(f64::min)
        .unwrap_or(now_ts);
    let dur_sec = (HUMIDITY_DURATION_MINUTES as f64) * 60.0;
    if (now_ts - start_ts) < dur_sec {
        return (t_guard, humidity_guard_states::THERMAL_SHIELDING);
    }
    let phi_proj = calculate_projected_base_humidity(climate.0, climate.1, base_temp);
    let wall_rh = climate.2.map_or(0.0, |t_out| {
        calculate_wall_surface_humidity(base_temp, phi_proj, t_out, DIN_4108_FRSI)
    });
    if wall_rh >= WALL_RH_STEPDOWN_SAFE_PCT {
        return (t_guard, humidity_guard_states::THERMAL_SHIELDING);
    }
    #[allow(clippy::cast_possible_truncation)]
    let steps =
        (((now_ts - start_ts) - dur_sec).max(0.0) / STEP_DOWN_INTERVAL_SECS).floor() as i32 + 1;
    let stepped = (t_guard - f64::from(steps) * STEP_DOWN_DELTA_C).max(base_temp);
    (stepped, humidity_guard_states::RECOVERY_CHECK)
}

// =============================================================================
/// Loads recent indoor `(timestamp, temp_c, rh_pct)` samples for `source_ain` from SQLite.
pub(super) async fn load_recent_sql_samples(
    source_ain: &str,
    cutoff: f64,
    now_ts: f64,
    db: &Database,
) -> Vec<ClimateSample> {
    if source_ain.is_empty() {
        return Vec::new();
    }
    let src = source_ain.to_owned();
    db.run_blocking(move |d| {
        let Ok(ent) = d.resolve_entity(&src) else {
            return Ok(Vec::new());
        };
        let rows = d.fetch_bounded_history_rows(ent.entity_id, cutoff, now_ts)?;
        Ok(rows
            .into_iter()
            .filter_map(|(ts, m)| {
                let tc = m.get("temperature_current")?.as_f64()?;
                let rh = m.get("humidity")?.as_f64()?;
                (tc > 0.0).then_some(ClimateSample {
                    timestamp: ts,
                    temp_c: tc,
                    rh_pct: rh,
                })
            })
            .collect::<Vec<_>>())
    })
    .await
    .unwrap_or_default()
}

// =============================================================================
/// Builds the `HumidityControl` per-scope diagnostic decision trace JSON object.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_humidity_scope_trace(
    config: &HumidityControlConfig,
    is_active: bool,
    targets: &[String],
    registry: &StateRegistry,
    weather_service: Option<&WeatherService>,
    scheduled_base_temp: Option<f64>,
    scope_states: &HashMap<String, &'static str>,
    previous_targets: &HashMap<String, f64>,
    boost_set: &HashSet<String>,
) -> Value {
    let mut bound = config.clone();
    bound.actor_ain = targets.to_vec();
    let (src_ain, temp_in, rh_in) = resolve_scope_climate(&bound, registry);
    let (temp_out, _) = outdoor_climate(weather_service);
    let base_t = resolve_scope_base_temp(&bound, registry, previous_targets, scheduled_base_temp);
    let skey = build_scope_key(&bound);
    let t_guard = temp_in.zip(rh_in).map(|(ti, ri)| {
        calculate_guard_temperature(&GuardTempParams {
            temp_in_c: ti,
            rel_humidity_in_pct: ri,
            temp_base_c: base_t,
            temp_out_c: temp_out,
            humidity_threshold: bound.humidity_threshold,
            hysteresis_percent: HUMIDITY_HYSTERESIS_PERCENT,
            max_temp_boost_c: bound.max_temp_boost_c,
            action_temp: HUMIDITY_ACTION_TEMP_CAP,
        })
    });
    let s_dev = if !src_ain.is_empty() {
        registry.get_device(&src_ain)
    } else {
        targets.first().and_then(|a| registry.get_device(a))
    };
    let mut boosting: Vec<String> = targets
        .iter()
        .map(|a| normalize_ain(a))
        .filter(|n| boost_set.contains(n))
        .collect();
    boosting.sort();
    json!({
        "is_active": is_active,
        "state": scope_states.get(&skey).copied().unwrap_or(humidity_guard_states::IDLE),
        "sensor_ain": bound.sensor_ain,
        "humidity_threshold": bound.humidity_threshold,
        "hysteresis_percent": HUMIDITY_HYSTERESIS_PERCENT,
        "max_temp_boost_c": bound.max_temp_boost_c,
        "action_temp": HUMIDITY_ACTION_TEMP_CAP,
        "sustain_minutes": HUMIDITY_SUSTAIN_MINUTES,
        "duration_minutes": HUMIDITY_DURATION_MINUTES,
        "base_temp_c": base_t,
        "guard_target_temp_c": t_guard,
        "dew_point_c": s_dev.as_ref().and_then(|d| d.dew_point_c),
        "abs_humidity_gm3": s_dev.as_ref().and_then(|d| d.abs_humidity_gm3),
        "wall_surface_humidity_pct": s_dev.as_ref().and_then(|d| d.wall_surface_humidity_pct),
        "projected_base_humidity_pct": s_dev.as_ref().and_then(|d| d.projected_base_humidity_pct),
        "ventilation_advice": s_dev.as_ref().and_then(|d| d.ventilation_advice.clone()),
        "boosting_ains": boosting,
    })
}
