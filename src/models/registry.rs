//! Thread-safe in-memory Digital Twin registry (`StateRegistry`).
//!
//! Acts as the Single Source of Truth for `axum` REST handlers, `FritzPollingWorker`,
//! and `RulesEngine`, answering `/api/v1/state` queries in microseconds without
//! synchronous FritzBox round-trips.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use serde_json::Value;

use crate::core::domain_constants::hardware::{device_types, fritz_api::SUBUNIT_DEFAULT_SUFFIX};
use crate::core::energy_analytics::EnergySummary;
use crate::core::time_util::{normalize_ain, now_local_iso8601};
use crate::models::device::DeviceState;
use crate::models::weather::WeatherSnapshot;

// =============================================================================
/// Compact live switch/meter telemetry snapshot used during `EnergySummary` assembly.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSwitchTelemetry {
    /// Whether the switch/meter is currently online.
    pub present: bool,
    /// Instantaneous active power (`W`).
    pub power_current: f64,
    /// Cumulative energy counter (`Wh`).
    pub energy_total: f64,
    /// Device capability list (`["power_meter", ...]`).
    pub capabilities: Vec<String>,
    /// Hardware product name (`"FRITZ!Smart Energy 250"`, etc.).
    pub product_name: Option<String>,
}

// =============================================================================
/// Internal mutable state protected by [`StateRegistry`]'s `RwLock`.
#[derive(Debug, Default)]
struct RegistryInner {
    /// Known Smart Home devices keyed by AIN in deterministic sorted order.
    devices: BTreeMap<String, DeviceState>,
    /// Cached house-wide 24h heating efficiency + 5-day forecast JSON block.
    heating_efficiency: Option<Value>,
    /// Cached house-wide smart energy consumption & cost summary.
    energy_summary: Option<EnergySummary>,
    /// Latest Open-Meteo outdoor weather snapshot.
    weather: Option<WeatherSnapshot>,
    /// Last raw `<tsoll>` values reported by the FritzBox per normalized AIN (dial-turn detection).
    raw_reported_targets: HashMap<String, f64>,
}

// =============================================================================
/// Thread-safe Digital Twin registry shared via `Arc<StateRegistry>` across tasks.
#[derive(Debug, Default)]
pub struct StateRegistry {
    /// Reader-writer lock allowing concurrent `GET /api/v1/state` reads without contention.
    inner: RwLock<RegistryInner>,
}

impl StateRegistry {
    /// Creates an empty [`StateRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or updates `state` under `ain`, stamping `last_updated`.
    pub fn update_device(&self, ain: &str, mut state: DeviceState) {
        state.last_updated = now_local_iso8601();
        if let Ok(mut guard) = self.inner.write() {
            guard.devices.insert(ain.to_owned(), state);
        }
    }

    /// Mutates an existing device in place (resolving space-normalized or `-1` HAN-FUN AINs).
    pub fn mutate_device<F>(&self, ain: &str, mutator: F) -> bool
    where
        F: FnOnce(&mut DeviceState),
    {
        let Ok(mut guard) = self.inner.write() else {
            return false;
        };
        let Some(key) = resolve_matching_key(&guard.devices, ain) else {
            return false;
        };
        if let Some(dev) = guard.devices.get_mut(&key) {
            mutator(dev);
            dev.last_updated = now_local_iso8601();
            return true;
        }
        false
    }

    /// Looks up a cloned [`DeviceState`] by exact, space-normalized, or HAN-FUN `-1` AIN.
    #[must_use]
    pub fn get_device(&self, ain: &str) -> Option<DeviceState> {
        let guard = self.inner.read().ok()?;
        let key = resolve_matching_key(&guard.devices, ain)?;
        guard.devices.get(&key).cloned()
    }

    /// Returns a cloned snapshot of all registered [`DeviceState`] instances.
    #[must_use]
    pub fn get_all_devices(&self) -> Vec<DeviceState> {
        self.inner
            .read()
            .map(|g| g.devices.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Removes a device by AIN, returning `true` if an entry existed.
    pub fn remove_device(&self, ain: &str) -> bool {
        let Ok(mut guard) = self.inner.write() else {
            return false;
        };
        if let Some(key) = resolve_matching_key(&guard.devices, ain) {
            return guard.devices.remove(&key).is_some();
        }
        false
    }

    /// Updates the cached house-wide 24h heating efficiency JSON summary.
    pub fn set_heating_efficiency(&self, efficiency: Option<Value>) {
        if let Ok(mut guard) = self.inner.write() {
            guard.heating_efficiency = efficiency;
        }
    }

    /// Retrieves the cached house-wide 24h heating efficiency JSON summary.
    #[must_use]
    pub fn get_heating_efficiency(&self) -> Option<Value> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.heating_efficiency.clone())
    }

    /// Updates the cached house-wide [`EnergySummary`].
    pub fn set_energy_summary(&self, summary: Option<EnergySummary>) {
        if let Ok(mut guard) = self.inner.write() {
            guard.energy_summary = summary;
        }
    }

    /// Retrieves the cached house-wide [`EnergySummary`].
    #[must_use]
    pub fn get_energy_summary(&self) -> Option<EnergySummary> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.energy_summary.clone())
    }

    /// Updates the cached outdoor [`WeatherSnapshot`].
    pub fn set_weather(&self, weather: Option<WeatherSnapshot>) {
        if let Ok(mut guard) = self.inner.write() {
            guard.weather = weather;
        }
    }

    /// Retrieves the cached outdoor [`WeatherSnapshot`].
    #[must_use]
    pub fn get_weather(&self) -> Option<WeatherSnapshot> {
        self.inner.read().ok().and_then(|g| g.weather.clone())
    }

    /// Records the latest raw hardware `<tsoll>` reported by the FritzBox for `ain`.
    pub fn record_raw_reported_target(&self, ain: &str, raw_target: f64) {
        if let Ok(mut guard) = self.inner.write() {
            guard
                .raw_reported_targets
                .insert(normalize_ain(ain), raw_target);
        }
    }

    /// Reads the previous raw hardware `<tsoll>` reported by the FritzBox for `ain`.
    #[must_use]
    pub fn get_raw_reported_target(&self, ain: &str) -> Option<f64> {
        let norm = normalize_ain(ain);
        self.inner
            .read()
            .ok()
            .and_then(|g| g.raw_reported_targets.get(&norm).copied())
    }

    /// Extracts `normalized_ain -> temperature_target` for all active thermostats in RAM.
    #[must_use]
    pub fn get_live_thermostat_targets(&self) -> HashMap<String, f64> {
        let Ok(guard) = self.inner.read() else {
            return HashMap::new();
        };
        guard
            .devices
            .values()
            .filter_map(|dev| {
                let tgt = dev.as_thermostat()?.temperature_target;
                (tgt > 0.0).then(|| (normalize_ain(&dev.ain), tgt))
            })
            .collect()
    }

    /// Extracts `normalized_ain -> LiveSwitchTelemetry` (including HAN-FUN base AIN aliases).
    #[must_use]
    pub fn get_live_switch_states(&self) -> HashMap<String, LiveSwitchTelemetry> {
        let Ok(guard) = self.inner.read() else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        for dev in guard.devices.values() {
            if dev.device_type != device_types::SWITCH {
                continue;
            }
            let Some(sw) = dev.as_switch() else {
                continue;
            };
            let snap = LiveSwitchTelemetry {
                present: dev.present,
                power_current: sw.power_current,
                energy_total: sw.energy_total,
                capabilities: dev.capabilities.clone(),
                product_name: dev.product_name.clone(),
            };
            let norm = normalize_ain(&dev.ain);
            if let Some((base, suffix)) = norm.rsplit_once('-') {
                if suffix.chars().all(|c| c.is_ascii_digit())
                    && base.chars().all(|c| c.is_ascii_digit())
                {
                    out.entry(base.to_owned()).or_insert_with(|| snap.clone());
                }
            }
            out.insert(norm, snap);
        }
        out
    }
}

// =============================================================================
/// Resolves a stored map key matching `ain` exactly, space-normalized, or via HAN-FUN `-1`.
fn resolve_matching_key(devices: &BTreeMap<String, DeviceState>, ain: &str) -> Option<String> {
    if devices.contains_key(ain) {
        return Some(ain.to_owned());
    }
    let norm = normalize_ain(ain);
    for key in devices.keys() {
        if normalize_ain(key) == norm {
            return Some(key.clone());
        }
    }
    let alt = if let Some((base, suffix)) = norm.rsplit_once('-') {
        (suffix.chars().all(|c| c.is_ascii_digit()) && base.chars().all(|c| c.is_ascii_digit()))
            .then_some(base.to_owned())
    } else if norm.chars().all(|c| c.is_ascii_digit()) && !norm.is_empty() {
        Some(format!("{norm}{SUBUNIT_DEFAULT_SUFFIX}"))
    } else {
        None
    }?;
    devices
        .keys()
        .find(|key| normalize_ain(key) == alt)
        .cloned()
}
