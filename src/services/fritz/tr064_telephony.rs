//! FritzBox `TR-064` Telephony: Answering Machine (`TAM`), Call List, and Phonebook.
//!
//! Separated from [`super::tr064_actions`] so WLAN/Host control and `X_AVM-DE_TAM` /
//! `X_AVM-DE_OnTel` telephony XML parsing each stay under `300` lines.

use std::collections::BTreeMap;

use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

use super::tr064::FritzTr064Client;
use super::tr064_actions::node_child_text;
use super::tr064_soap::is_tr064_enabled;

// =============================================================================
/// Answering machine (`TAM`) summary returned in `GET /api/v1/system/calls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweringMachineStatus {
    /// Whether the primary answering machine (`index 0`) is enabled.
    pub enabled: bool,
    /// Display name (`"Anrufbeantworter 1"` or `"N/A"`).
    pub name: String,
}

// =============================================================================
/// Normalized telephony call log item matching `/api/v1/system/calls` in `openapi.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEntry {
    /// FritzBox call entry ID.
    pub id: String,
    /// Call direction/state code (`"1"` incoming, `"2"` missed, `"3"` outgoing).
    #[serde(rename = "type")]
    pub call_type: String,
    /// Calling party number or fallback identifier.
    pub caller: String,
    /// Dialed / called number.
    pub called: String,
    /// Resolved phonebook contact name, if known.
    pub name: String,
    /// Formatted timestamp string (`"dd.mm.yy HH:MM"` / `"YYYY-MM-DD HH:MM"`).
    pub date: String,
    /// Call duration (`"H:MM"`).
    pub duration: String,
}

impl FritzTr064Client {
    /// Queries the status of the primary answering machine (`X_AVM-DE_TAM:1#GetInfo`).
    pub async fn get_answering_machine_status(&self) -> AnsweringMachineStatus {
        if self.ensure_connected().await.is_err() {
            return AnsweringMachineStatus {
                enabled: false,
                name: String::from("N/A"),
            };
        }
        match self
            .call_action("X_AVM-DE_TAM:1", "GetInfo", &[("NewIndex", "0")])
            .await
        {
            Ok(info) => AnsweringMachineStatus {
                enabled: is_tr064_enabled(info.get("NewEnable").map(String::as_str)),
                name: info
                    .get("NewName")
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| String::from("N/A")),
            },
            Err(err) => {
                self.logger
                    .warning(&format!("Failed to get TAM status: {err}"));
                AnsweringMachineStatus {
                    enabled: false,
                    name: String::from("N/A"),
                }
            }
        }
    }

    /// Fetches and normalizes up to `limit` recent calls via `X_AVM-DE_OnTel:1#GetCallList`.
    pub async fn get_call_list(&self, limit: usize) -> Vec<CallEntry> {
        if self.ensure_connected().await.is_err() {
            return Vec::new();
        }
        let Ok(res) = self
            .call_action("X_AVM-DE_OnTel:1", "GetCallList", &[])
            .await
        else {
            return Vec::new();
        };
        let Some(raw_url) = res.get("NewCallListURL").filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        let sep = if raw_url.contains('?') { '&' } else { '?' };
        let bounded_url = format!("{raw_url}{sep}max={limit}");
        match self.download_tr064_url(&bounded_url).await {
            Ok(xml) => parse_call_list_xml(&xml, limit),
            Err(err) => {
                self.logger
                    .warning(&format!("Failed to fetch call list XML: {err}"));
                Vec::new()
            }
        }
    }

    /// Fetches `name -> Vec<number>` contacts from phonebook `phonebook_id`.
    pub async fn get_phonebook(&self, phonebook_id: u32) -> BTreeMap<String, Vec<String>> {
        if self.ensure_connected().await.is_err() {
            return BTreeMap::new();
        }
        let id_str = phonebook_id.to_string();
        let Ok(res) = self
            .call_action(
                "X_AVM-DE_OnTel:1",
                "GetPhonebook",
                &[("NewPhonebookID", &id_str)],
            )
            .await
        else {
            return BTreeMap::new();
        };
        let Some(url) = res.get("NewPhonebookURL").filter(|s| !s.is_empty()) else {
            return BTreeMap::new();
        };
        match self.download_tr064_url(url).await {
            Ok(xml) => parse_phonebook_xml(&xml),
            Err(err) => {
                self.logger
                    .warning(&format!("Failed to fetch phonebook {phonebook_id}: {err}"));
                BTreeMap::new()
            }
        }
    }
}

// =============================================================================
/// Parses up to `limit` [`CallEntry`] records from `X_AVM-DE_OnTel` call-list XML.
fn parse_call_list_xml(xml: &str, limit: usize) -> Vec<CallEntry> {
    let Ok(doc) = Document::parse(xml) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Call")
        .take(limit)
        .map(|call| {
            let called = first_child_str(call, &["Called", "called"]);
            let name = first_child_str(call, &["Name", "name"]);
            let caller = first_child_str(
                call,
                &[
                    "Caller",
                    "caller",
                    "CallerNumber",
                    "Called",
                    "called",
                    "Name",
                    "name",
                ],
            );
            CallEntry {
                id: first_child_str(call, &["Id", "id"]),
                call_type: first_child_str(call, &["Type", "type"]),
                caller,
                called,
                name,
                date: first_child_str(call, &["Date", "date"]),
                duration: first_child_str(call, &["Duration", "duration"]),
            }
        })
        .collect()
}

// =============================================================================
/// Parses `contact -> Vec<number>` entries from AVM phonebook XML.
fn parse_phonebook_xml(xml: &str) -> BTreeMap<String, Vec<String>> {
    let Ok(doc) = Document::parse(xml) else {
        return BTreeMap::new();
    };
    let mut contacts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for contact in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "contact")
    {
        let Some(real_name) = contact
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "realName")
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let numbers: Vec<String> = contact
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "number")
            .filter_map(|n| n.text().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_owned)
            .collect();
        contacts
            .entry(real_name.to_owned())
            .or_default()
            .extend(numbers);
    }
    contacts
}

// =============================================================================
/// Reads trimmed non-empty text of the first matching child tag in `tags`.
fn first_child_str(node: Node<'_, '_>, tags: &[&str]) -> String {
    for &tag in tags {
        if let Some(txt) = node_child_text(node, tag) {
            return txt.to_owned();
        }
    }
    String::new()
}
