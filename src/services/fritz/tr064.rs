//! Pure-Rust FritzBox `TR-064` / `IGD` SOAP client (`port 49000`).
//!
//! Coordinates SCPD service discovery (`/tr64desc.xml` & `/igddesc.xml`), HTTP Digest
//! authenticated SOAP 1.1 dispatch, router metadata (`DeviceInfo:1`), WAN link/IP/traffic
//! queries, WAN reconnect (`ForceTermination`), and router reboot (`DeviceConfig:1`).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use super::auth::{build_aha_base_url, build_tr064_base_url, DigestAuthenticator};
use super::tr064_soap::{
    build_soap_envelope, clean_meta_value, decode_soap_response, default_avm_service_map,
    is_valid_public_ip, merge_scpd_xml, parse_u64_field, ScpdCatalog, Tr064ServiceDesc,
};
use crate::core::constants::service_defaults::DEFAULT_FRITZBOX_TIMEOUT_SECS;
use crate::core::domain_constants::hardware::{
    fritz_api::WAN_SERVICES,
    tr064_defaults::{DEFAULT_MANUFACTURER, SCPD_DISCOVERY_PATHS, UNKNOWN_META},
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::validation::sanitize_log_uri;

// =============================================================================
/// Hot-reloadable TR-064 connection credentials and base URLs.
#[derive(Debug, Clone)]
pub(super) struct Tr064EndpointConfig {
    /// TR-064 HTTP base URL (`"http://fritz.box:49000"`).
    pub tr064_base: String,
    /// Standard HTTP base URL (`"http://fritz.box"`) for relative download paths.
    pub http_base: String,
    /// FritzBox username.
    pub username: String,
    /// FritzBox password.
    pub password: String,
}

// =============================================================================
/// FritzBox hardware & firmware metadata matching `router_info` in `openapi.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterInfo {
    /// Hardware model (`"FRITZ!Box 7590"`).
    pub model: String,
    /// CPE manufacturer (`"AVM"`).
    pub manufacturer: String,
    /// FRITZ!OS version (`"7.57"`).
    pub software_version: String,
    /// Device serial number or `"Unknown"`.
    pub serial_number: String,
}

// =============================================================================
/// Live WAN link, IP, uptime, and traffic counters matching `connection_status` in `openapi.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionStatus {
    /// Physical DSL/Fiber/Cable link state.
    pub is_linked: bool,
    /// Logical WAN IP/PPP session state.
    pub is_connected: bool,
    /// Router or WAN session uptime in seconds.
    pub uptime: u64,
    /// Assigned public WAN IPv4 or IPv6 address.
    pub external_ip: Option<String>,
    /// Cumulative WAN bytes transmitted.
    pub bytes_sent: u64,
    /// Cumulative WAN bytes received.
    pub bytes_received: u64,
}

// =============================================================================
/// Async Pure-Rust FritzBox TR-064 SOAP client safe for concurrent sharing via `Arc`.
#[derive(Debug)]
pub struct FritzTr064Client {
    /// Pooled `rustls` HTTP client.
    pub(super) http: Client,
    /// Hot-reloadable host and credentials.
    pub(super) endpoint: RwLock<Tr064EndpointConfig>,
    /// Stateful HTTP Digest authenticator (`RFC 2617` / `RFC 7616`).
    pub(super) digest: DigestAuthenticator,
    /// Discovered SCPD service catalog.
    pub(super) catalog: AsyncMutex<ScpdCatalog>,
    /// Fallback firmware version injected from AHA `getdevicelistinfos` XML.
    fallback_fw_version: RwLock<Option<String>>,
    /// Application logger with secret redaction.
    pub(super) logger: Logger,
}

impl FritzTr064Client {
    /// Constructs a new [`FritzTr064Client`] targeting `host` on port `49000`.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if building the HTTP client fails.
    pub fn new(host: &str, username: &str, password: &str, logger: Logger) -> AppResult<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_FRITZBOX_TIMEOUT_SECS))
            .build()
            .map_err(|e| AppError::Service(format!("Failed building TR-064 HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoint: RwLock::new(Tr064EndpointConfig {
                tr064_base: build_tr064_base_url(host),
                http_base: build_aha_base_url(host),
                username: username.to_owned(),
                password: password.to_owned(),
            }),
            digest: DigestAuthenticator::new(),
            catalog: AsyncMutex::new(ScpdCatalog::default()),
            fallback_fw_version: RwLock::new(None),
            logger,
        })
    }

    /// Updates FritzBox host/credentials on `SIGHUP` reload and clears cached auth/SCPD state.
    pub async fn reconfigure(&self, host: &str, username: &str, password: &str) {
        let new_tr064 = build_tr064_base_url(host);
        let new_http = build_aha_base_url(host);
        let mut changed = false;
        if let Ok(mut cfg) = self.endpoint.write() {
            changed =
                cfg.tr064_base != new_tr064 || cfg.username != username || cfg.password != password;
            cfg.tr064_base = new_tr064;
            cfg.http_base = new_http;
            cfg.username = username.to_owned();
            cfg.password = password.to_owned();
        }
        if changed {
            self.digest.clear();
            let mut cat = self.catalog.lock().await;
            *cat = ScpdCatalog::default();
        }
    }

    /// Stores a fallback firmware version extracted from AHA `getdevicelistinfos` XML.
    pub fn set_fallback_firmware(&self, version: Option<&str>) {
        if let Some(cleaned) = clean_meta_value(version) {
            if let Ok(mut guard) = self.fallback_fw_version.write() {
                *guard = Some(cleaned);
            }
        }
    }

    /// Discovers SCPD service descriptions (`/tr64desc.xml` & `/igddesc.xml`).
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if the FritzBox TR-064 port is completely unreachable.
    pub async fn connect(&self) -> AppResult<()> {
        let mut guard = self.catalog.lock().await;
        let cfg = self.read_endpoint()?;
        let mut cat = ScpdCatalog {
            initialized: true,
            services: default_avm_service_map(),
            desc_model: None,
            desc_version: None,
        };
        let mut any_ok = false;
        for path in SCPD_DISCOVERY_PATHS {
            let url = format!("{}{path}", cfg.tr064_base);
            if let Ok(resp) = self.http.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(xml) = resp.text().await {
                        merge_scpd_xml(&xml, &mut cat);
                        any_ok = true;
                    }
                }
            }
        }
        if !any_ok {
            return Err(AppError::Service(format!(
                "Cannot connect to FritzBox TR-064 on {}",
                cfg.tr064_base
            )));
        }
        self.logger.debug(&format!(
            "Connected to FritzBox TR-064 on {}",
            cfg.tr064_base
        ));
        *guard = cat;
        Ok(())
    }

    /// Queries router hardware & firmware metadata (`DeviceInfo:1#GetInfo` + SCPD/AHA fallbacks).
    pub async fn get_device_info(&self) -> Option<RouterInfo> {
        if self.ensure_connected().await.is_err() {
            return None;
        }
        let info = self
            .call_action("DeviceInfo:1", "GetInfo", &[])
            .await
            .unwrap_or_default();
        let (desc_model, desc_ver) = {
            let cat = self.catalog.lock().await;
            (cat.desc_model.clone(), cat.desc_version.clone())
        };
        let fb_fw = self.fallback_fw_version.read().ok().and_then(|g| g.clone());
        let model = clean_meta_value(info.get("NewModelName").map(String::as_str)).or(desc_model);
        let sw_ver = clean_meta_value(info.get("NewSoftwareVersion").map(String::as_str))
            .or(desc_ver)
            .or(fb_fw);
        if info.is_empty() && model.is_none() && sw_ver.is_none() {
            return None;
        }
        let manufacturer = clean_meta_value(info.get("NewManufacturerName").map(String::as_str))
            .unwrap_or_else(|| DEFAULT_MANUFACTURER.to_owned());
        let serial = clean_meta_value(info.get("NewSerialNumber").map(String::as_str))
            .unwrap_or_else(|| UNKNOWN_META.to_owned());
        Some(RouterInfo {
            model: model.unwrap_or_else(|| UNKNOWN_META.to_owned()),
            manufacturer,
            software_version: sw_ver.unwrap_or_else(|| UNKNOWN_META.to_owned()),
            serial_number: serial,
        })
    }

    /// Resolves the active public WAN IPv4 (with IPv6 / DS-Lite and IGD fallback).
    pub async fn get_external_ip(&self) -> Option<String> {
        if self.ensure_connected().await.is_err() {
            return None;
        }
        for &svc in &[
            "WANIPConn:1",
            "WANIPConnection:1",
            "WANPPPConnection:1",
            "WANPPPConn:1",
        ] {
            if let Ok(map) = self.call_action(svc, "GetExternalIPAddress", &[]).await {
                if let Some(ip) = map
                    .get("NewExternalIPAddress")
                    .filter(|s| is_valid_public_ip(s))
                {
                    return Some(ip.trim().to_owned());
                }
            }
        }
        for &svc in &["WANIPConn:1", "WANIPConnection:1", "WANPPPConnection:1"] {
            if let Ok(map) = self
                .call_action(svc, "X_AVM-DE_GetExternalIPv6Address", &[])
                .await
            {
                if let Some(ip6) = map
                    .get("NewExternalIPv6Address")
                    .filter(|s| is_valid_public_ip(s))
                {
                    return Some(ip6.trim().to_owned());
                }
            }
        }
        None
    }

    /// Queries live WAN link, connection status, uptime, public IP, and traffic byte counters.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if TR-064 discovery fails completely.
    pub async fn get_system_status(&self) -> AppResult<ConnectionStatus> {
        self.ensure_connected().await?;
        let dev_info = self
            .call_action("DeviceInfo:1", "GetInfo", &[])
            .await
            .unwrap_or_default();
        let dev_uptime = parse_u64_field(&dev_info, "NewUpTime");
        let (wan_uptime, wan_connected) = self.query_wan_status_info().await;
        let uptime = if dev_uptime > 0 {
            dev_uptime
        } else {
            wan_uptime
        };
        let external_ip = self.get_external_ip().await;
        let linked_opt = self.query_physical_link_up().await;
        let (bytes_sent, bytes_received) = self.query_traffic_counters().await;
        let is_linked = linked_opt
            .unwrap_or_else(|| external_ip.is_some() || uptime > 0 || !dev_info.is_empty());
        let is_connected = wan_connected.unwrap_or(false) || external_ip.is_some() || is_linked;
        Ok(ConnectionStatus {
            is_linked,
            is_connected,
            uptime,
            external_ip,
            bytes_sent,
            bytes_received,
        })
    }

    /// Triggers WAN termination and public IP renewal across TR-064/IGD WAN services.
    ///
    /// # Errors
    /// Returns [`AppError`] if every `ForceTermination` candidate fails.
    pub async fn reconnect_internet(&self) -> AppResult<()> {
        self.ensure_connected().await?;
        let mut last_err: Option<AppError> = None;
        for &svc in WAN_SERVICES {
            match self.call_action(svc, "ForceTermination", &[]).await {
                Ok(_) => {
                    self.logger
                        .info(&format!("Triggered Internet reconnect via {svc}"));
                    return Ok(());
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            AppError::Service("Failed to reconnect internet: no WAN service responded".into())
        }))
    }

    /// Reboots the FritzBox CPE via `DeviceConfig:1#Reboot`.
    ///
    /// # Errors
    /// Returns [`AppError`] if the TR-064 reboot call fails.
    pub async fn reboot_router(&self) -> AppResult<()> {
        self.ensure_connected().await?;
        self.call_action("DeviceConfig:1", "Reboot", &[]).await?;
        self.logger.warning("Triggered FritzBox reboot");
        Ok(())
    }

    /// Ensures SCPD service discovery has been executed at least once.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if initial connection fails.
    pub(super) async fn ensure_connected(&self) -> AppResult<()> {
        let initialized = self.catalog.lock().await.initialized;
        if !initialized {
            self.connect().await?;
        }
        Ok(())
    }

    /// Invokes a TR-064 / IGD SOAP action with HTTP Digest authentication.
    ///
    /// # Errors
    /// Returns [`AppError::Auth`] on credential rejection or [`AppError::Service`] on SOAP fault.
    pub(super) async fn call_action(
        &self,
        service: &str,
        action: &str,
        args: &[(&str, &str)],
    ) -> AppResult<HashMap<String, String>> {
        let desc = self.resolve_service(service).await?;
        let cfg = self.read_endpoint()?;
        let envelope = build_soap_envelope(&desc.service_type, action, args);
        let soap_action = format!("\"{}#{action}\"", desc.service_type);
        let pre_auth =
            self.digest
                .preemptive_header(&cfg.username, &cfg.password, "POST", &desc.control_url);
        let resp = self
            .send_soap_post(
                &cfg.tr064_base,
                &desc.control_url,
                &soap_action,
                &envelope,
                pre_auth,
            )
            .await?;

        if resp.status().as_u16() == 401 {
            let www_auth = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| AppError::Auth("TR-064 returned 401 without challenge".into()))?
                .to_owned();
            let auth_hdr = self.digest.handle_unauthorized(
                &www_auth,
                &cfg.username,
                &cfg.password,
                "POST",
                &desc.control_url,
            )?;
            let retry_resp = self
                .send_soap_post(
                    &cfg.tr064_base,
                    &desc.control_url,
                    &soap_action,
                    &envelope,
                    Some(auth_hdr),
                )
                .await?;
            return decode_soap_response(retry_resp, service, action).await;
        }
        decode_soap_response(resp, service, action).await
    }

    /// Resolves a short service identifier (`"DeviceInfo:1"`) into [`Tr064ServiceDesc`].
    async fn resolve_service(&self, service: &str) -> AppResult<Tr064ServiceDesc> {
        let cat = self.catalog.lock().await;
        cat.services
            .get(service)
            .cloned()
            .ok_or_else(|| AppError::Service(format!("TR-064 service '{service}' unavailable")))
    }

    /// Sends a single HTTP `POST` carrying a SOAP XML envelope.
    async fn send_soap_post(
        &self,
        base_url: &str,
        control_url: &str,
        soap_action: &str,
        body: &str,
        auth_header: Option<String>,
    ) -> AppResult<reqwest::Response> {
        let url = format!("{base_url}{control_url}");
        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", soap_action)
            .body(body.to_owned());
        if let Some(hdr) = auth_header {
            req = req.header("Authorization", hdr);
        }
        req.send().await.map_err(|e| {
            let safe = sanitize_log_uri(&e.to_string());
            AppError::Service(format!("TR-064 POST '{control_url}' failed: {safe}"))
        })
    }

    /// Reads [`Tr064EndpointConfig`] safely.
    pub(super) fn read_endpoint(&self) -> AppResult<Tr064EndpointConfig> {
        self.endpoint
            .read()
            .map(|g| g.clone())
            .map_err(|_| AppError::Service("TR-064 endpoint lock poisoned".into()))
    }

    /// Queries WAN uptime and connection flag across `WAN_SERVICES`.
    async fn query_wan_status_info(&self) -> (u64, Option<bool>) {
        for &svc in WAN_SERVICES {
            if let Ok(info) = self.call_action(svc, "GetStatusInfo", &[]).await {
                let status = info
                    .get("NewConnectionStatus")
                    .map(|s| s.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                let uptime = parse_u64_field(&info, "NewUptime");
                if !status.is_empty() || uptime > 0 {
                    return (uptime, Some(status == "connected" || uptime > 0));
                }
            }
        }
        (0, None)
    }

    /// Queries physical WAN link status (`Up` / `Down`) via `WANCommonIFC1` / `WANCommonInterfaceConfig:1`.
    async fn query_physical_link_up(&self) -> Option<bool> {
        for &svc in &["WANCommonIFC:1", "WANCommonInterfaceConfig:1"] {
            if let Ok(info) = self.call_action(svc, "GetCommonLinkProperties", &[]).await {
                if let Some(st) = info.get("NewPhysicalLinkStatus") {
                    return Some(st.trim().eq_ignore_ascii_case("up"));
                }
            }
        }
        None
    }

    /// Queries 64-bit or 32-bit WAN byte counters via IGD `GetAddonInfos` / `GetTotalBytes*`.
    async fn query_traffic_counters(&self) -> (u64, u64) {
        if let Ok(addon) = self
            .call_action("WANCommonIFC:1", "GetAddonInfos", &[])
            .await
        {
            let sent = parse_u64_field(&addon, "NewX_AVM_DE_TotalBytesSent64")
                .max(parse_u64_field(&addon, "NewTotalBytesSent"));
            let recv = parse_u64_field(&addon, "NewX_AVM_DE_TotalBytesReceived64")
                .max(parse_u64_field(&addon, "NewTotalBytesReceived"));
            if sent > 0 || recv > 0 {
                return (sent, recv);
            }
        }
        for &svc in &["WANCommonIFC:1", "WANCommonInterfaceConfig:1"] {
            let sent = self
                .call_action(svc, "GetTotalBytesSent", &[])
                .await
                .map_or(0, |m| parse_u64_field(&m, "NewTotalBytesSent"));
            let recv = self
                .call_action(svc, "GetTotalBytesReceived", &[])
                .await
                .map_or(0, |m| parse_u64_field(&m, "NewTotalBytesReceived"));
            if sent > 0 || recv > 0 {
                return (sent, recv);
            }
        }
        (0, 0)
    }
}
