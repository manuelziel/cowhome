//! TR-064 SOAP 1.1 framing, fault extraction, and SCPD XML service discovery helpers.
//!
//! Separated from [`super::tr064`] so client orchestration and XML/SOAP protocol
//! serialization each stay concise (`<= 450` lines).

use std::collections::HashMap;

use roxmltree::{Document, Node};

use crate::core::domain_constants::hardware::tr064_defaults::{INVALID_WAN_IPS, UNKNOWN_META};
use crate::core::error::{AppError, AppResult};

// =============================================================================
/// Discovered or fallback TR-064 / UPnP SOAP service endpoint descriptor.
#[derive(Debug, Clone)]
pub(super) struct Tr064ServiceDesc {
    /// Full URN service type (`"urn:dslforum-org:service:DeviceInfo:1"`).
    pub service_type: String,
    /// Relative HTTP POST path (`"/upnp/control/deviceinfo"`).
    pub control_url: String,
}

// =============================================================================
/// Cached SCPD discovery result from `/tr64desc.xml` and `/igddesc.xml`.
#[derive(Debug, Clone, Default)]
pub(super) struct ScpdCatalog {
    /// Whether live SCPD XML discovery has completed at least once.
    pub initialized: bool,
    /// Map of short service name (`"DeviceInfo:1"`) to SOAP endpoint metadata.
    pub services: HashMap<String, Tr064ServiceDesc>,
    /// Fallback router model extracted from `<tr64desc.xml>`.
    pub desc_model: Option<String>,
    /// Fallback system firmware version extracted from `<tr64desc.xml>`.
    pub desc_version: Option<String>,
}

// =============================================================================
/// Decodes an HTTP SOAP response into output key-value pairs or a structured error.
///
/// # Errors
/// Returns [`AppError::Auth`] on `401`/`403` or [`AppError::Service`] on SOAP fault / XML error.
pub(super) async fn decode_soap_response(
    resp: reqwest::Response,
    service: &str,
    action: &str,
) -> AppResult<HashMap<String, String>> {
    let code = resp.status().as_u16();
    if code == 401 || code == 403 {
        return Err(AppError::Auth(format!(
            "TR-064 '{service}#{action}' rejected (HTTP {code})"
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| AppError::Service(format!("TR-064 '{service}#{action}' read error: {e}")))?;
    if !(200..300).contains(&code) {
        return Err(AppError::Service(format!(
            "TR-064 '{service}#{action}' failed (HTTP {code}): {}",
            extract_soap_fault(&xml)
        )));
    }
    let doc = Document::parse(&xml).map_err(|e| {
        AppError::Service(format!("Invalid SOAP XML from '{service}#{action}': {e}"))
    })?;
    let resp_tag = format!("{action}Response");
    let Some(resp_node) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == resp_tag)
    else {
        return Ok(HashMap::new());
    };
    Ok(resp_node
        .children()
        .filter(Node::is_element)
        .map(|c| {
            (
                c.tag_name().name().to_owned(),
                c.text().unwrap_or("").trim().to_owned(),
            )
        })
        .collect())
}

// =============================================================================
/// Extracts a human-readable fault description from a SOAP `<Fault>` XML body.
fn extract_soap_fault(xml: &str) -> String {
    let Ok(doc) = Document::parse(xml) else {
        return String::from("SOAP error");
    };
    let find_tag = |name: &str| {
        doc.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match (
        find_tag("errorCode"),
        find_tag("errorDescription").or_else(|| find_tag("faultstring")),
    ) {
        (Some(c), Some(d)) => format!("{c} {d}"),
        (None, Some(d)) => d.to_owned(),
        (Some(c), None) => format!("UPnP error {c}"),
        (None, None) => String::from("SOAP fault"),
    }
}

// =============================================================================
/// Builds a SOAP 1.1 request XML envelope with entity-escaped parameters.
#[must_use]
pub(super) fn build_soap_envelope(
    service_type: &str,
    action: &str,
    args: &[(&str, &str)],
) -> String {
    let mut params_xml = String::new();
    for &(name, val) in args {
        params_xml.push_str(&format!("<{name}>{}</{name}>", escape_xml(val)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{params_xml}</u:{action}></s:Body>\
         </s:Envelope>"
    )
}

// =============================================================================
/// Escapes XML special characters inside SOAP argument values.
fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// =============================================================================
/// Merges `<service>` definitions and fallback device metadata from an SCPD XML document.
pub(super) fn merge_scpd_xml(xml: &str, cat: &mut ScpdCatalog) {
    let Ok(doc) = Document::parse(xml) else {
        return;
    };
    if cat.desc_version.is_none() {
        cat.desc_version = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "systemVersion")
            .and_then(|sv| {
                sv.children()
                    .find(|c| c.is_element() && c.tag_name().name() == "Display")
            })
            .and_then(|d| clean_meta_value(d.text()));
    }
    if cat.desc_model.is_none() {
        cat.desc_model = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "friendlyName")
            .and_then(|n| clean_meta_value(n.text()));
    }
    for svc in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "service")
    {
        register_scpd_service_node(svc, cat);
    }
}

// =============================================================================
/// Extracts `<serviceType>`, `<controlURL>`, and `<serviceId>` from a single `<service>` node.
fn register_scpd_service_node(svc: Node<'_, '_>, cat: &mut ScpdCatalog) {
    let child_txt = |tag: &str| {
        svc.children()
            .find(|c| c.is_element() && c.tag_name().name() == tag)
            .and_then(|c| c.text())
            .map(str::trim)
            .unwrap_or("")
    };
    let (stype, ctrl, sid) = (
        child_txt("serviceType"),
        child_txt("controlURL"),
        child_txt("serviceId"),
    );
    if let Some(short) = extract_short_service_name(stype) {
        cat.services.insert(
            short,
            Tr064ServiceDesc {
                service_type: stype.to_owned(),
                control_url: ctrl.to_owned(),
            },
        );
    }
    if sid.ends_with("WANIPConn1") {
        cat.services.insert(
            String::from("WANIPConn:1"),
            Tr064ServiceDesc {
                service_type: stype.to_owned(),
                control_url: ctrl.to_owned(),
            },
        );
    } else if sid.ends_with("WANCommonIFC1") {
        cat.services.insert(
            String::from("WANCommonIFC:1"),
            Tr064ServiceDesc {
                service_type: stype.to_owned(),
                control_url: ctrl.to_owned(),
            },
        );
    }
}

// =============================================================================
/// Extracts `"ServiceName:Version"` from `"urn:...:service:ServiceName:Version"`.
fn extract_short_service_name(service_type: &str) -> Option<String> {
    let mut rev = service_type.rsplitn(3, ':');
    let ver = rev.next()?;
    let name = rev.next()?;
    (!name.is_empty() && !ver.is_empty()).then(|| format!("{name}:{ver}"))
}

// =============================================================================
/// Static AVM FRITZ!OS fallback service table when SCPD XML omits individual nodes.
#[must_use]
pub(super) fn default_avm_service_map() -> HashMap<String, Tr064ServiceDesc> {
    const TABLE: &[(&str, &str, &str)] = &[
        (
            "DeviceInfo:1",
            "urn:dslforum-org:service:DeviceInfo:1",
            "/upnp/control/deviceinfo",
        ),
        (
            "DeviceConfig:1",
            "urn:dslforum-org:service:DeviceConfig:1",
            "/upnp/control/deviceconfig",
        ),
        (
            "WLANConfiguration:1",
            "urn:dslforum-org:service:WLANConfiguration:1",
            "/upnp/control/wlanconfig1",
        ),
        (
            "WLANConfiguration:2",
            "urn:dslforum-org:service:WLANConfiguration:2",
            "/upnp/control/wlanconfig2",
        ),
        (
            "WLANConfiguration:3",
            "urn:dslforum-org:service:WLANConfiguration:3",
            "/upnp/control/wlanconfig3",
        ),
        (
            "Hosts:1",
            "urn:dslforum-org:service:Hosts:1",
            "/upnp/control/hosts",
        ),
        (
            "X_AVM-DE_TAM:1",
            "urn:dslforum-org:service:X_AVM-DE_TAM:1",
            "/upnp/control/x_tam",
        ),
        (
            "X_AVM-DE_OnTel:1",
            "urn:dslforum-org:service:X_AVM-DE_OnTel:1",
            "/upnp/control/x_contact",
        ),
        (
            "WANIPConnection:1",
            "urn:dslforum-org:service:WANIPConnection:1",
            "/upnp/control/wanipconnection1",
        ),
        (
            "WANPPPConnection:1",
            "urn:dslforum-org:service:WANPPPConnection:1",
            "/upnp/control/wanpppconnection1",
        ),
        (
            "WANIPConn:1",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "/igdupnp/control/WANIPConn1",
        ),
        (
            "WANCommonIFC:1",
            "urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1",
            "/igdupnp/control/WANCommonIFC1",
        ),
        (
            "WANCommonInterfaceConfig:1",
            "urn:dslforum-org:service:WANCommonInterfaceConfig:1",
            "/upnp/control/wancommonifconfig1",
        ),
    ];
    TABLE
        .iter()
        .map(|&(k, st, url)| {
            (
                k.to_owned(),
                Tr064ServiceDesc {
                    service_type: st.to_owned(),
                    control_url: url.to_owned(),
                },
            )
        })
        .collect()
}

// =============================================================================
/// Trims router metadata strings and filters empty or `"Unknown"` placeholders.
#[must_use]
pub(super) fn clean_meta_value(val: Option<&str>) -> Option<String> {
    let trimmed = val?.trim();
    (!trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(UNKNOWN_META)).then(|| trimmed.to_owned())
}

// =============================================================================
/// Returns `true` when `ip` is a non-empty assigned WAN IPv4/IPv6 address.
#[must_use]
pub(super) fn is_valid_public_ip(ip: &str) -> bool {
    let trimmed = ip.trim();
    !INVALID_WAN_IPS.contains(&trimmed)
}

// =============================================================================
/// Parses a `u64` integer from `map[key]`, defaulting to `0`.
#[must_use]
pub(super) fn parse_u64_field(map: &HashMap<String, String>, key: &str) -> u64 {
    map.get(key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

// =============================================================================
/// Normalizes TR-064 boolean flags (`"1"`, `"true"`, `"yes"`, `"on"` -> `true`).
#[must_use]
pub(super) fn is_tr064_enabled(val: Option<&str>) -> bool {
    let Some(raw) = val.map(str::trim) else {
        return false;
    };
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
