//! FritzBox AHA `getdevicelistinfos` XML parser and HAN-FUN / ETSI sub-unit cross-linker.
//!
//! Parses raw `<devicelist>` XML via `roxmltree` without intermediate object allocations,
//! ignores FritzBox-internal hardware groups (`<group>` / `grp*`), cross-links HAN-FUN
//! base devices with their ETSI sub-units (e.g., `FRITZ!Smart Energy 250`), and resolves
//! standardized Digital Twin capability tokens.

use std::collections::{BTreeMap, HashSet};

use roxmltree::{Document, Node};

use crate::core::domain_constants::hardware::{
    device_capabilities, fritz_api, fritz_bitmask, fritz_xml_tags as tags,
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::normalize_ain;

// =============================================================================
/// Complete result of parsing a FritzBox `getdevicelistinfos` XML payload.
#[derive(Debug, Clone, Default)]
pub struct ParsedDeviceList {
    /// Optional FritzBox OS firmware version extracted from `<devicelist fwversion="...">`.
    pub firmware_version: Option<String>,
    /// Physical Smart Home devices ready for Digital Twin construction (groups/shells removed).
    pub devices: Vec<RawAhaDevice>,
    /// HAN-FUN base container AINs suppressed because a richer ETSI sub-unit is present.
    pub suppressed_shell_ains: Vec<String>,
}

// =============================================================================
/// Normalized physical device telemetry extracted from `<device>` and HAN-FUN cross-linking.
#[derive(Debug, Clone, PartialEq)]
pub struct RawAhaDevice {
    /// Original hardware AIN string (`"15282 0905546"` or `"14276 0590081-1"`).
    pub ain: String,
    /// Space-stripped AIN key (`"152820905546"`).
    pub normalized_ain: String,
    /// Internal numeric FritzBox device ID (`id="..."`), if present.
    pub device_id: Option<String>,
    /// Parent base device ID reported inside `<etsiunitinfo><etsideviceid>`.
    pub etsi_device_id: Option<String>,
    /// Linked HAN-FUN parent normalized AIN, if cross-linked.
    pub parent_ain: Option<String>,
    /// Linked ETSI sub-unit normalized AIN when this node is a HAN-FUN base device.
    pub subunit_ain: Option<String>,
    /// Hardware-reported device name (`<name>`).
    pub name: String,
    /// Reachability state (`<present>1</present>`).
    pub present: bool,
    /// Original `functionbitmask` before HAN-FUN bitmask merging.
    pub raw_bitmask: u32,
    /// Effective `functionbitmask` (merged with sub-unit flags for HAN-FUN base devices).
    pub bitmask: u32,
    /// Hardware product model (`"FRITZ!DECT 301"`, `"FRITZ!Smart Energy 250"`, etc.).
    pub product_name: Option<String>,
    /// Device firmware version (`fwversion`), inherited from HAN-FUN parent when `"0.0"`/empty.
    pub fw_version: Option<String>,
    /// Battery charge (`0`..=`100 %`), inherited from HAN-FUN parent when omitted on sub-unit.
    pub battery_level: Option<u8>,
    /// Hardware low-battery warning flag (`<batterylow>1</batterylow>`).
    pub battery_low: bool,
    /// Ambient/radiator temperature in `°C` from `<temperature><celsius>`.
    pub temperature: Option<f64>,
    /// Internal FritzBox hardware offset in `°C` from `<temperature><offset>`.
    pub temperature_offset: Option<f64>,
    /// Relative humidity (`%`) from `<humidity><rel_humidity>`.
    pub humidity: Option<f64>,
    /// Radiator valve temperature in `°C` from `<hkr><tist>`.
    pub hkr_temp: Option<f64>,
    /// Decoded target temperature in `°C` from `<hkr><tsoll>` (`0.0` = OFF, `28.0` = ON).
    pub target_temperature: Option<f64>,
    /// Open-window active flag (`<hkr><windowopenactiv>1</windowopenactiv>`).
    pub window_open: bool,
    /// Boost mode active flag (`<hkr><boostactive>1</boostactive>`).
    pub boost_active: bool,
    /// Numeric AVM HKR valve error code (`0`..=`6`).
    pub error_code: u8,
    /// Switch/bulb/alert binary state (`<switch>`, `<simpleonoff>`, or `<alert>`).
    pub switch_state: Option<bool>,
    /// True when a `<powermeter>` child block exists (or was inherited from HAN-FUN sub-unit).
    pub has_powermeter: bool,
    /// Mains voltage in `V` rounded to `0.1 V`.
    pub voltage: Option<f64>,
    /// Active power in `mW` (signed: negative during solar PV feed-in).
    pub power_mw: Option<i64>,
    /// Cumulative energy counter in `Wh`.
    pub energy_wh: Option<i64>,
    /// True when one or more `<button>` child elements exist.
    pub has_button: bool,
    /// Most recent physical button press Unix timestamp (`s`).
    pub last_pressed: Option<i64>,
}

impl RawAhaDevice {
    /// Resolves CowHome's canonical `capabilities` list for this device.
    #[must_use]
    pub fn resolve_capabilities(&self, resolved_humidity: Option<f64>) -> Vec<String> {
        let mask = self.bitmask;
        let has_thermo = (mask & fritz_bitmask::THERMOSTAT) != 0 || self.hkr_temp.is_some();
        let has_temp = has_thermo
            || (mask & fritz_bitmask::TEMPERATURE_SENSOR) != 0
            || self.temperature.is_some();
        let has_bat = has_thermo || self.battery_level.is_some() || self.battery_low;
        let has_pm = self.has_powermeter || (mask & fritz_bitmask::POWER_METER) != 0;
        let has_sw = (mask & fritz_bitmask::SWITCH) != 0;
        let has_hum = (mask & fritz_bitmask::HUMIDITY_SENSOR) != 0 || resolved_humidity.is_some();
        let has_win = has_thermo || (mask & fritz_bitmask::ALARM_SENSOR) != 0;
        let has_btn = (mask & fritz_bitmask::BUTTON) != 0 || self.has_button;

        let pairs = [
            (has_thermo, device_capabilities::THERMOSTAT),
            (has_sw, device_capabilities::SWITCH),
            (has_pm, device_capabilities::POWER_METER),
            (has_temp, device_capabilities::TEMPERATURE_SENSOR),
            (has_hum, device_capabilities::HUMIDITY_SENSOR),
            (has_bat, device_capabilities::BATTERY),
            (has_win, device_capabilities::WINDOW_DETECTION),
            (has_btn, device_capabilities::BUTTON),
            (
                (mask & fritz_bitmask::LIGHTBULB) != 0,
                device_capabilities::LIGHTBULB,
            ),
            (
                (mask & fritz_bitmask::COLOR_CONTROL) != 0,
                device_capabilities::COLOR_CONTROL,
            ),
            (
                (mask & fritz_bitmask::LEVEL_CONTROL) != 0,
                device_capabilities::LEVEL_CONTROL,
            ),
            (
                (mask & fritz_bitmask::BLIND) != 0,
                device_capabilities::BLIND,
            ),
            (
                (mask & fritz_bitmask::ALARM_SENSOR) != 0,
                device_capabilities::ALARM_SENSOR,
            ),
            (
                (mask & fritz_bitmask::REPEATER) != 0,
                device_capabilities::REPEATER,
            ),
        ];
        pairs
            .into_iter()
            .filter(|&(active, _)| active)
            .map(|(_, cap)| cap.to_owned())
            .collect()
    }
}

// =============================================================================
/// Parses a complete `getdevicelistinfos` XML document into [`ParsedDeviceList`].
///
/// # Errors
/// Returns [`AppError::Service`] if `xml` is malformed XML.
pub fn parse_device_list_xml(xml: &str) -> AppResult<ParsedDeviceList> {
    let doc = Document::parse(xml)
        .map_err(|e| AppError::Service(format!("Invalid AHA devicelist XML: {e}")))?;
    let root = doc.root_element();
    let firmware_version = root
        .attribute(tags::FW_VERSION)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut map: BTreeMap<String, RawAhaDevice> = BTreeMap::new();
    for node in root.children().filter(Node::is_element) {
        if node.tag_name().name() != tags::DEVICE {
            continue;
        }
        let raw_ain = node.attribute(tags::IDENTIFIER).unwrap_or("").trim();
        if raw_ain.is_empty() || raw_ain.starts_with(fritz_api::GROUP_AIN_PREFIX) {
            continue;
        }
        let dev = parse_device_node(node, raw_ain);
        map.insert(dev.normalized_ain.clone(), dev);
    }

    cross_link_hanfun_units(&mut map);
    let present_norms: HashSet<String> = map.keys().cloned().collect();
    let mut devices = Vec::with_capacity(map.len());
    let mut suppressed_shell_ains = Vec::new();

    for (_, dev) in map {
        let is_shell = dev.raw_bitmask == fritz_bitmask::HAN_FUN_DEVICE
            && dev
                .subunit_ain
                .as_ref()
                .is_some_and(|sub| present_norms.contains(sub));
        if is_shell {
            suppressed_shell_ains.push(dev.ain);
        } else {
            devices.push(dev);
        }
    }

    Ok(ParsedDeviceList {
        firmware_version,
        devices,
        suppressed_shell_ains,
    })
}

// =============================================================================
/// Parses a single `<device>` XML element into [`RawAhaDevice`].
fn parse_device_node(node: Node<'_, '_>, raw_ain: &str) -> RawAhaDevice {
    let raw_mask = node
        .attribute(tags::FUNCTION_BITMASK)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let etsi_id = child_elem(node, tags::ETSI_UNIT_INFO)
        .and_then(|e| child_text(e, tags::ETSI_DEVICE_ID))
        .map(str::to_owned);

    let mut dev = RawAhaDevice {
        ain: raw_ain.to_owned(),
        normalized_ain: normalize_ain(raw_ain),
        device_id: non_empty_attr(node, tags::ID),
        etsi_device_id: etsi_id,
        parent_ain: None,
        subunit_ain: None,
        name: child_text(node, tags::NAME).unwrap_or("Unknown").to_owned(),
        present: child_text(node, tags::PRESENT) == Some("1"),
        raw_bitmask: raw_mask,
        bitmask: raw_mask,
        product_name: non_empty_attr(node, tags::PRODUCT_NAME),
        fw_version: non_empty_attr(node, tags::FW_VERSION),
        battery_level: child_text(node, tags::BATTERY).and_then(|s| s.parse::<u8>().ok()),
        battery_low: child_text(node, tags::BATTERY_LOW) == Some("1"),
        temperature: None,
        temperature_offset: None,
        humidity: None,
        hkr_temp: None,
        target_temperature: None,
        window_open: false,
        boost_active: false,
        error_code: 0,
        switch_state: parse_binary_state(node),
        has_powermeter: false,
        voltage: None,
        power_mw: None,
        energy_wh: None,
        has_button: false,
        last_pressed: None,
    };
    populate_climate_fields(node, &mut dev);
    populate_hkr_fields(node, &mut dev);
    populate_power_and_buttons(node, &mut dev);
    dev
}

// =============================================================================
/// Populates `<temperature>` and `<humidity>` telemetry on `dev`.
fn populate_climate_fields(node: Node<'_, '_>, dev: &mut RawAhaDevice) {
    if let Some(temp_node) = child_elem(node, tags::TEMPERATURE) {
        if let Some(raw_c) =
            child_text(temp_node, tags::CELSIUS).and_then(|s| s.parse::<i32>().ok())
        {
            dev.temperature = Some(f64::from(raw_c) / fritz_api::DECICELSIUS_FACTOR);
        }
        if let Some(raw_off) =
            child_text(temp_node, tags::OFFSET).and_then(|s| s.parse::<i32>().ok())
        {
            dev.temperature_offset = Some(f64::from(raw_off) / fritz_api::DECICELSIUS_FACTOR);
        }
    }
    if let Some(hum_node) = child_elem(node, tags::HUMIDITY) {
        if let Some(rh) =
            child_text(hum_node, tags::REL_HUMIDITY).and_then(|s| s.parse::<u8>().ok())
        {
            dev.humidity = Some(f64::from(rh));
        }
    }
}

// =============================================================================
/// Populates `<hkr>` radiator valve telemetry (including nested battery fallbacks) on `dev`.
fn populate_hkr_fields(node: Node<'_, '_>, dev: &mut RawAhaDevice) {
    let Some(hkr) = child_elem(node, tags::HKR) else {
        return;
    };
    if let Some(tist) = child_text(hkr, tags::TIST).and_then(|s| s.parse::<i32>().ok()) {
        dev.hkr_temp = Some(f64::from(tist) / fritz_api::HALF_DEGREE_FACTOR);
    }
    if let Some(tsoll) = child_text(hkr, tags::TSOLL).and_then(|s| s.parse::<u16>().ok()) {
        dev.target_temperature = Some(match tsoll {
            fritz_api::TSOLL_OFF_RAW => 0.0,
            fritz_api::TSOLL_ON_RAW => fritz_api::MAX_THERMOSTAT_TEMP,
            steps => f64::from(steps) / fritz_api::HALF_DEGREE_FACTOR,
        });
    }
    dev.window_open = child_text(hkr, tags::WINDOW_OPEN_ACTIVE) == Some("1");
    dev.boost_active = child_text(hkr, tags::BOOST_ACTIVE) == Some("1");
    dev.error_code = child_text(hkr, tags::ERROR_CODE)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0);
    if dev.battery_level.is_none() {
        dev.battery_level = child_text(hkr, tags::BATTERY).and_then(|s| s.parse::<u8>().ok());
    }
    if !dev.battery_low && child_text(hkr, tags::BATTERY_LOW) == Some("1") {
        dev.battery_low = true;
    }
}

// =============================================================================
/// Populates `<powermeter>` and `<button>` telemetry on `dev`.
fn populate_power_and_buttons(node: Node<'_, '_>, dev: &mut RawAhaDevice) {
    if let Some(pm) = child_elem(node, tags::POWERMETER) {
        dev.has_powermeter = true;
        if let Some(mv) = child_text(pm, tags::VOLTAGE).and_then(|s| s.parse::<u32>().ok()) {
            let volts = f64::from(mv) / fritz_api::MILLIVOLTS_PER_VOLT;
            dev.voltage = Some((volts * 10.0).round() / 10.0);
        }
        dev.power_mw = child_text(pm, tags::POWER).and_then(|s| s.parse::<i64>().ok());
        dev.energy_wh = child_text(pm, tags::ENERGY).and_then(|s| s.parse::<i64>().ok());
    }
    let mut max_press: Option<i64> = None;
    for btn in node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == tags::BUTTON)
    {
        dev.has_button = true;
        if let Some(ts) = child_text(btn, tags::LAST_PRESSED)
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|&v| v > 0)
        {
            max_press = Some(max_press.map_or(ts, |cur| cur.max(ts)));
        }
    }
    dev.last_pressed = max_press;
}

// =============================================================================
/// Extracts `<state>` (`0` / `1`) from `<switch>`, `<simpleonoff>`, or `<alert>`.
fn parse_binary_state(node: Node<'_, '_>) -> Option<bool> {
    for block_tag in [tags::SWITCH, tags::SIMPLE_ON_OFF, tags::ALERT] {
        if let Some(block) = child_elem(node, block_tag) {
            match child_text(block, tags::STATE) {
                Some("1") => return Some(true),
                Some("0") => return Some(false),
                _ => {}
            }
        }
    }
    None
}

// =============================================================================
/// Cross-links HAN-FUN parent containers with ETSI sub-units (`Smart Energy 250`, `DECT 440`).
fn cross_link_hanfun_units(map: &mut BTreeMap<String, RawAhaDevice>) {
    let by_id: BTreeMap<String, String> = map
        .iter()
        .filter_map(|(ain, d)| d.device_id.as_ref().map(|id| (id.clone(), ain.clone())))
        .collect();

    let pairs: Vec<(String, String)> = map
        .iter()
        .filter_map(|(ain, dev)| {
            let parent = dev
                .etsi_device_id
                .as_ref()
                .and_then(|eid| by_id.get(eid).cloned())
                .or_else(|| {
                    let (base, suffix) = ain.rsplit_once('-')?;
                    (suffix.chars().all(|c| c.is_ascii_digit()) && map.contains_key(base))
                        .then(|| base.to_owned())
                })?;
            (parent != *ain && map.contains_key(&parent)).then_some((parent, ain.clone()))
        })
        .collect();

    for (parent_ain, child_ain) in pairs {
        let (Some(p_snap), Some(c_snap)) =
            (map.get(&parent_ain).cloned(), map.get(&child_ain).cloned())
        else {
            continue;
        };
        if let Some(child) = map.get_mut(&child_ain) {
            child.parent_ain = Some(parent_ain.clone());
            if child.battery_level.is_none() {
                child.battery_level = p_snap.battery_level;
            }
            child.battery_low = child.battery_low || p_snap.battery_low;
            if child.product_name.is_none() {
                child.product_name = p_snap.product_name.clone();
            }
            if matches!(child.fw_version.as_deref(), None | Some("" | "0.0")) {
                child.fw_version = p_snap.fw_version.clone();
            }
        }
        if p_snap.raw_bitmask == fritz_bitmask::HAN_FUN_DEVICE {
            if let Some(parent) = map.get_mut(&parent_ain) {
                parent.subunit_ain = Some(child_ain);
                parent.bitmask |= c_snap.bitmask;
                parent.has_powermeter = parent.has_powermeter || c_snap.has_powermeter;
                if parent.power_mw.is_none() {
                    parent.power_mw = c_snap.power_mw;
                }
                if parent.energy_wh.is_none() {
                    parent.energy_wh = c_snap.energy_wh;
                }
                if parent.voltage.is_none() {
                    parent.voltage = c_snap.voltage;
                }
                if parent.switch_state.is_none() {
                    parent.switch_state = c_snap.switch_state;
                }
            }
        }
    }
}

// =============================================================================
/// Finds the first direct child element of `node` matching `tag`.
fn child_elem<'a, 'input>(node: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == tag)
}

// =============================================================================
/// Reads trimmed non-empty text of the first direct child element matching `tag`.
fn child_text<'a>(node: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    child_elem(node, tag)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

// =============================================================================
/// Reads a trimmed non-empty XML attribute on `node`.
fn non_empty_attr(node: Node<'_, '_>, attr: &str) -> Option<String> {
    node.attribute(attr)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
