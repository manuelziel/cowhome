//! Virtual backend software-group models (`software_groups` & `software_group_members`).
//!
//! CowHome ignores FritzBox-internal `<group>` nodes completely and manages room groups
//! with 100% CRUD authority in SQLite (`group-1`, `group-2`, ...).

use serde::{Deserialize, Serialize};

use crate::core::domain_constants::hardware::special_devices::GROUP_ID_PREFIX;

// =============================================================================
/// Virtual room group combining thermostats, climate sensors, or smart switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoftwareGroup {
    /// Numeric SQLite primary key (`1`, `2`, ...).
    pub group_id: i64,
    /// Canonical string identifier exposed in REST responses (`"group-1"`).
    pub id: String,
    /// User-assigned room name (`"Wohnzimmer"`).
    pub name: String,
    /// Member device AINs belonging to this room group.
    pub members: Vec<String>,
    /// Synchronized room target temperature (`°C`) derived from member thermostats.
    pub target_temperature: Option<f64>,
    /// Backend-managed group comfort preset temperature (`°C`).
    pub comfort_temperature: Option<f64>,
    /// Backend-managed group eco setback temperature (`°C`).
    pub eco_temperature: Option<f64>,
    /// SQLite creation timestamp.
    pub created_at: String,
    /// SQLite last-modified timestamp.
    pub updated_at: String,
}

// =============================================================================
/// Formats a numeric `group_id` into its canonical REST identifier (`"group-1"`).
#[must_use]
pub fn format_group_id(group_id: i64) -> String {
    format!("{GROUP_ID_PREFIX}{group_id}")
}

// =============================================================================
/// Parses either `"group-1"` or `"1"` into a positive numeric `group_id`.
#[must_use]
pub fn parse_group_id(raw: &str) -> Option<i64> {
    let stripped = raw.trim().trim_start_matches(GROUP_ID_PREFIX).trim();
    stripped.parse::<i64>().ok().filter(|&id| id > 0)
}
