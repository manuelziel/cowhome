//! FritzBox `TR-064` WLAN, LAN/WLAN host discovery, and Wake-on-LAN actions.
//!
//! Extends [`FritzTr064Client`] with dual-band main/guest Wi-Fi control, fast XML host
//! list retrieval (`X_AVM-DE_GetHostListPath` + `GetGenericHostEntry` fallback), and
//! Wake-on-LAN magic-packet dispatch (`Hosts:1#X_AVM-DE_WakeOnLANByMACAddress`).

use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

use super::tr064::FritzTr064Client;
use super::tr064_soap::is_tr064_enabled;
use crate::core::domain_constants::hardware::fritz_api::{
    GUEST_WLAN_INDEX, MAIN_WLAN_INDEX, SECONDARY_WLAN_INDEX,
};
use crate::core::error::{AppError, AppResult};
use crate::core::validation::sanitize_log_uri;

// =============================================================================
/// Main Wi-Fi status returned in `GET /api/v1/system/status` (`wlan_main`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WlanStatus {
    /// Whether main Wi-Fi (`2.4 GHz` / `5 GHz`) is enabled.
    pub enabled: bool,
    /// Broadcast Wi-Fi SSID.
    pub ssid: String,
    /// Active radio channel (`0` when disabled or unavailable).
    pub channel: u16,
}

// =============================================================================
/// Guest Wi-Fi status returned in `GET /api/v1/system/status` (`wlan_guest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestWlanStatus {
    /// Whether guest Wi-Fi is enabled.
    pub enabled: bool,
    /// Guest Wi-Fi SSID.
    pub ssid: String,
}

// =============================================================================
/// Active LAN/WLAN network host entry matching `/api/v1/system/hosts` in `openapi.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    /// Assigned IPv4 address (`"192.168.178.20"`).
    pub ip: String,
    /// Hostname reported by DHCP / FritzBox (`"Pixel-Phone"`).
    pub name: String,
    /// Hardware MAC address (`"00:11:22:33:44:55"`).
    pub mac: String,
    /// Reachability flag (`true` for active hosts).
    pub status: bool,
    /// Physical medium (`"802.11"`, `"Ethernet"`, etc.).
    pub interface_type: String,
    /// Address assignment method (`"DHCP"`, `"Static"`, etc.).
    pub address_source: String,
}

impl FritzTr064Client {
    /// Queries combined main Wi-Fi (`2.4 GHz` / `5 GHz`) status.
    ///
    /// # Errors
    /// Returns [`AppError`] if querying `WLANConfiguration` fails.
    pub async fn get_wlan_status(&self) -> AppResult<WlanStatus> {
        self.ensure_connected().await?;
        let primary = self.query_wlan_index(MAIN_WLAN_INDEX).await?;
        let count = self.wlan_service_count().await;
        if primary.enabled || count < GUEST_WLAN_INDEX {
            return Ok(primary);
        }
        if let Ok(secondary) = self.query_wlan_index(SECONDARY_WLAN_INDEX).await
            && secondary.enabled
        {
            return Ok(secondary);
        }
        Ok(primary)
    }

    /// Enables or disables main Wi-Fi (`2.4 GHz` and `5 GHz` on dual-band models).
    ///
    /// # Errors
    /// Returns [`AppError`] if setting `WLANConfiguration` fails.
    pub async fn set_wlan(&self, enabled: bool) -> AppResult<()> {
        self.ensure_connected().await?;
        let flag = if enabled { "1" } else { "0" };
        let svc1 = format!("WLANConfiguration:{MAIN_WLAN_INDEX}");
        self.call_action(&svc1, "SetEnable", &[("NewEnable", flag)])
            .await?;
        if self.wlan_service_count().await >= GUEST_WLAN_INDEX {
            let svc2 = format!("WLANConfiguration:{SECONDARY_WLAN_INDEX}");
            if let Err(err) = self
                .call_action(&svc2, "SetEnable", &[("NewEnable", flag)])
                .await
            {
                self.logger
                    .debug(&format!("Secondary 5GHz WLAN toggle skipped: {err}"));
            }
        }
        self.logger.info(&format!(
            "Set main WLAN to {}",
            if enabled { "ON" } else { "OFF" }
        ));
        Ok(())
    }

    /// Queries guest Wi-Fi status (highest `WLANConfiguration` index).
    pub async fn get_guest_wlan_status(&self) -> GuestWlanStatus {
        if self.ensure_connected().await.is_err() {
            return GuestWlanStatus {
                enabled: false,
                ssid: String::new(),
            };
        }
        let guest_idx = self.wlan_service_count().await.max(SECONDARY_WLAN_INDEX);
        match self.query_wlan_index(guest_idx).await {
            Ok(info) => GuestWlanStatus {
                enabled: info.enabled,
                ssid: info.ssid,
            },
            Err(err) => {
                self.logger.debug(&format!(
                    "Guest WLAN unavailable on index {guest_idx}: {err}"
                ));
                GuestWlanStatus {
                    enabled: false,
                    ssid: String::new(),
                }
            }
        }
    }

    /// Enables or disables guest Wi-Fi (`WLANConfiguration:{guest_index}#SetEnable`).
    ///
    /// # Errors
    /// Returns [`AppError`] if toggling guest Wi-Fi fails.
    pub async fn set_guest_wlan(&self, enabled: bool) -> AppResult<()> {
        self.ensure_connected().await?;
        let guest_idx = self.wlan_service_count().await.max(SECONDARY_WLAN_INDEX);
        let flag = if enabled { "1" } else { "0" };
        let svc = format!("WLANConfiguration:{guest_idx}");
        self.call_action(&svc, "SetEnable", &[("NewEnable", flag)])
            .await?;
        self.logger.info(&format!(
            "Set guest WLAN to {}",
            if enabled { "ON" } else { "OFF" }
        ));
        Ok(())
    }

    /// Fetches all currently active LAN/WLAN hosts for presence profiling & `/system/hosts`.
    ///
    /// # Errors
    /// Returns [`AppError`] if both `X_AVM-DE_GetHostListPath` and `GetGenericHostEntry` fail.
    pub async fn get_connected_hosts(&self) -> AppResult<Vec<HostEntry>> {
        self.ensure_connected().await?;
        if let Ok(map) = self
            .call_action("Hosts:1", "X_AVM-DE_GetHostListPath", &[])
            .await
            && let Some(path) = map
                .get("NewX_AVM-DE_HostListPath")
                .filter(|s| !s.is_empty())
            && let Ok(xml) = self.download_tr064_url(path).await
            && let Ok(hosts) = parse_host_list_xml(&xml)
        {
            return Ok(hosts);
        }
        self.fetch_hosts_generic_fallback().await
    }

    /// Sends a Wake-on-LAN magic packet via `Hosts:1#X_AVM-DE_WakeOnLANByMACAddress`.
    ///
    /// # Errors
    /// Returns [`AppError`] if the TR-064 Wake-on-LAN action fails.
    pub async fn wake_on_lan(&self, mac_address: &str) -> AppResult<()> {
        self.ensure_connected().await?;
        self.call_action(
            "Hosts:1",
            "X_AVM-DE_WakeOnLANByMACAddress",
            &[("NewMACAddress", mac_address.trim())],
        )
        .await?;
        self.logger
            .info(&format!("Sent WakeOnLan to {}", mac_address.trim()));
        Ok(())
    }

    /// Downloads an XML resource returned by a TR-064 URL field (`:49000` with `:80` fallback).
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if the HTTP download or UTF-8 decode fails on all bases.
    pub(super) async fn download_tr064_url(&self, raw_url: &str) -> AppResult<String> {
        let cfg = self.read_endpoint()?;
        if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
            return self.fetch_sanitized_xml(raw_url).await;
        }
        let sep = if raw_url.starts_with('/') { "" } else { "/" };
        let tr064_url = format!("{}{sep}{raw_url}", cfg.tr064_base);
        match self.fetch_sanitized_xml(&tr064_url).await {
            Ok(xml) => Ok(xml),
            Err(_) if cfg.http_base != cfg.tr064_base => {
                let http_url = format!("{}{sep}{raw_url}", cfg.http_base);
                self.fetch_sanitized_xml(&http_url).await
            }
            Err(err) => Err(err),
        }
    }

    /// Performs a single sanitized `GET` download for a TR-064 XML resource.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] on transport failure, non-2xx HTTP status, or decode error.
    async fn fetch_sanitized_xml(&self, full_url: &str) -> AppResult<String> {
        let resp = self.http.get(full_url).send().await.map_err(|e| {
            let safe = sanitize_log_uri(&e.to_string());
            AppError::Service(format!("TR-064 XML download failed: {safe}"))
        })?;
        if !resp.status().is_success() {
            return Err(AppError::Service(format!(
                "TR-064 XML download returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        resp.text().await.map_err(|e| {
            let safe = sanitize_log_uri(&e.to_string());
            AppError::Service(format!("TR-064 XML body decode failed: {safe}"))
        })
    }

    /// Counts discovered `WLANConfiguration:*` services (defaulting to `GUEST_WLAN_INDEX`).
    async fn wlan_service_count(&self) -> u8 {
        let cat = self.catalog.lock().await;
        let count = cat
            .services
            .keys()
            .filter(|k| k.starts_with("WLANConfiguration:"))
            .count();
        u8::try_from(count)
            .ok()
            .filter(|&c| c > 0)
            .unwrap_or(GUEST_WLAN_INDEX)
    }

    /// Queries a single 1-based `WLANConfiguration:{index}#GetInfo` service.
    async fn query_wlan_index(&self, index: u8) -> AppResult<WlanStatus> {
        let svc = format!("WLANConfiguration:{index}");
        let info = self.call_action(&svc, "GetInfo", &[]).await?;
        let channel = info
            .get("NewChannel")
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(0);
        Ok(WlanStatus {
            enabled: is_tr064_enabled(info.get("NewEnable").map(String::as_str)),
            ssid: info.get("NewSSID").cloned().unwrap_or_default(),
            channel,
        })
    }

    /// Fallback host enumeration using `Hosts:1#GetHostNumberOfEntries` + `GetGenericHostEntry`.
    async fn fetch_hosts_generic_fallback(&self) -> AppResult<Vec<HostEntry>> {
        let count_map = self
            .call_action("Hosts:1", "GetHostNumberOfEntries", &[])
            .await?;
        let total = count_map
            .get("NewHostNumberOfEntries")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0)
            .min(512);
        let mut active = Vec::new();
        for idx in 0..total {
            let idx_str = idx.to_string();
            let Ok(entry) = self
                .call_action("Hosts:1", "GetGenericHostEntry", &[("NewIndex", &idx_str)])
                .await
            else {
                continue;
            };
            if is_tr064_enabled(entry.get("NewActive").map(String::as_str)) {
                active.push(HostEntry {
                    ip: entry.get("NewIPAddress").cloned().unwrap_or_default(),
                    name: entry.get("NewHostName").cloned().unwrap_or_default(),
                    mac: entry.get("NewMACAddress").cloned().unwrap_or_default(),
                    status: true,
                    interface_type: entry.get("NewInterfaceType").cloned().unwrap_or_default(),
                    address_source: entry.get("NewAddressSource").cloned().unwrap_or_default(),
                });
            }
        }
        Ok(active)
    }
}

// =============================================================================
/// Parses active [`HostEntry`] items from AVM's `devicehostlist.lua` XML.
fn parse_host_list_xml(xml: &str) -> AppResult<Vec<HostEntry>> {
    let doc = Document::parse(xml)
        .map_err(|e| AppError::Service(format!("Invalid host list XML: {e}")))?;
    let mut hosts = Vec::new();
    for item in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Item")
    {
        if !is_tr064_enabled(node_child_text(item, "Active")) {
            continue;
        }
        hosts.push(HostEntry {
            ip: node_child_text(item, "IPAddress").unwrap_or("").to_owned(),
            name: node_child_text(item, "HostName").unwrap_or("").to_owned(),
            mac: node_child_text(item, "MACAddress").unwrap_or("").to_owned(),
            status: true,
            interface_type: node_child_text(item, "InterfaceType")
                .unwrap_or("")
                .to_owned(),
            address_source: node_child_text(item, "AddressSource")
                .unwrap_or("")
                .to_owned(),
        });
    }
    Ok(hosts)
}

// =============================================================================
/// Reads trimmed non-empty text of a direct child element `tag`.
pub(super) fn node_child_text<'a>(node: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == tag)
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
