//! Virtual Software Groups CRUD and Single-Authority exclusive membership enforcement.
//!
//! Manages backend-exclusive room groups (`software_groups` & `software_group_members`),
//! enforces 1-group-per-device exclusivity (`HTTP 409 Conflict`), and revokes standalone
//! device automations when joining a group (`Single Authority Pattern`).

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::Database;
use crate::core::domain_constants::automation::automation_identifiers::{
    SCOPE_DEVICE, SCOPE_GROUP,
};
use crate::core::error::{AppError, AppResult};
use crate::core::time_util::{build_group_lww_key, normalize_ain, now_epoch_f64};
use crate::models::group::{format_group_id, parse_group_id};

impl Database {
    /// Creates a new virtual software group, enforcing exclusive membership and revoking
    /// standalone device automations (`Single Authority Pattern`).
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] if `name` is blank, [`AppError::Conflict`] if any
    /// member already belongs to another group, or [`AppError::Database`] on SQL error.
    pub fn create_software_group(
        &self,
        name: &str,
        members: &[String],
        effective_ts: Option<f64>,
    ) -> AppResult<Value> {
        let clean_name = name.trim();
        if clean_name.is_empty() {
            return Err(AppError::Validation(
                "Group 'name' is required and must be non-empty".into(),
            ));
        }
        let clean_members = sanitize_member_list(members);
        self.validate_exclusive_members(&clean_members, None)?;
        let init_ts = effective_ts.unwrap_or(0.0);

        let group_id = self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO software_groups (name, updated_at) VALUES (?, datetime(?, 'unixepoch'))",
                params![clean_name, init_ts],
            )?;
            let gid = tx.last_insert_rowid();
            insert_group_members_and_revoke(tx, gid, &clean_members)?;
            Ok(gid)
        })?;
        if init_ts > 0.0 {
            self.record_lww_mutation(&[build_group_lww_key(group_id)], init_ts);
        }
        let gid_str = group_id.to_string();
        Ok(self.get_software_group(&gid_str)?.unwrap_or_else(|| {
            json!({
                "id": gid_str,
                "name": clean_name,
                "members": clean_members,
                "automation": {},
            })
        }))
    }

    /// Fetches all virtual software groups, their sorted member AINs, and group automations.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `software_groups` fails.
    pub fn get_all_software_groups(&self) -> AppResult<Vec<Value>> {
        let (groups, mut members_map) = self.with_conn(|conn| {
            let mut g_stmt =
                conn.prepare("SELECT group_id, name FROM software_groups ORDER BY group_id ASC")?;
            let g_rows: Vec<(i64, String)> = g_stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut m_map: HashMap<i64, Vec<String>> = HashMap::new();
            if !g_rows.is_empty() {
                let mut m_stmt = conn
                    .prepare("SELECT group_id, ain FROM software_group_members ORDER BY ain ASC")?;
                for pair in
                    m_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                {
                    let (gid, ain) = pair?;
                    m_map.entry(gid).or_default().push(ain);
                }
            }
            Ok((g_rows, m_map))
        })?;

        let mut out = Vec::with_capacity(groups.len());
        for (gid, gname) in groups {
            let gid_str = gid.to_string();
            let auto = self.get_all_automation_configs(SCOPE_GROUP, &gid_str)?;
            out.push(json!({
                "id": gid_str,
                "name": gname,
                "members": members_map.remove(&gid).unwrap_or_default(),
                "automation": auto,
            }));
        }
        Ok(out)
    }

    /// Fetches a single software group by `"1"` or `"group-1"`, or `Ok(None)` if not found.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying SQLite fails.
    pub fn get_software_group(&self, group_id_raw: &str) -> AppResult<Option<Value>> {
        let Some(num_id) = parse_group_id(group_id_raw) else {
            return Ok(None);
        };
        let found = self.with_conn(|conn| {
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT group_id, name FROM software_groups WHERE group_id = ?",
                    params![num_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((gid, gname)) = row else {
                return Ok(None);
            };
            let mut m_stmt = conn.prepare(
                "SELECT ain FROM software_group_members WHERE group_id = ? ORDER BY ain ASC",
            )?;
            let members: Vec<String> = m_stmt
                .query_map(params![gid], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(Some((gid, gname, members)))
        })?;

        let Some((gid, gname, members)) = found else {
            return Ok(None);
        };
        let gid_str = gid.to_string();
        let auto = self.get_all_automation_configs(SCOPE_GROUP, &gid_str)?;
        Ok(Some(json!({
            "id": gid_str,
            "name": gname,
            "members": members,
            "automation": auto,
        })))
    }

    /// Updates `name` and/or `members` of an existing software group with LWW protection.
    ///
    /// Returns `Ok(None)` when the group does not exist, or `Ok(Some(obj))` including
    /// `"ignored_stale": true | false`.
    ///
    /// # Errors
    /// Returns [`AppError::Conflict`] on duplicate membership or [`AppError::Database`].
    pub fn update_software_group(
        &self,
        group_id_raw: &str,
        name: Option<&str>,
        members: Option<&[String]>,
        effective_ts: Option<f64>,
    ) -> AppResult<Option<Value>> {
        let Some(num_id) = parse_group_id(group_id_raw) else {
            return Ok(None);
        };
        let Some(mut existing) = self.get_software_group(&num_id.to_string())? else {
            return Ok(None);
        };
        let eff_ts = effective_ts.unwrap_or_else(now_epoch_f64);
        let lww_key = build_group_lww_key(num_id);
        if self.is_lww_stale(std::slice::from_ref(&lww_key), eff_ts) {
            if let Some(obj) = existing.as_object_mut() {
                obj.insert(String::from("ignored_stale"), json!(true));
            }
            return Ok(Some(existing));
        }
        let clean_members = members.map(sanitize_member_list);
        if let Some(ref list) = clean_members {
            self.validate_exclusive_members(list, Some(num_id))?;
        }
        self.apply_group_update_tx(num_id, name, clean_members.as_deref(), eff_ts)?;
        self.record_lww_mutation(&[lww_key], eff_ts);

        let mut updated = self
            .get_software_group(&num_id.to_string())?
            .unwrap_or(existing);
        if let Some(obj) = updated.as_object_mut() {
            obj.insert(String::from("ignored_stale"), json!(false));
        }
        Ok(Some(updated))
    }

    /// Executes the atomic `software_groups` & `software_group_members` update transaction.
    fn apply_group_update_tx(
        &self,
        num_id: i64,
        name: Option<&str>,
        clean_members: Option<&[String]>,
        eff_ts: f64,
    ) -> AppResult<()> {
        self.with_tx(|tx| {
            if let Some(raw_name) = name {
                tx.execute(
                    "UPDATE software_groups SET name = ?, updated_at = datetime(?, 'unixepoch') WHERE group_id = ?",
                    params![raw_name.trim(), eff_ts, num_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE software_groups SET updated_at = datetime(?, 'unixepoch') WHERE group_id = ?",
                    params![eff_ts, num_id],
                )?;
            }
            if let Some(list) = clean_members {
                tx.execute(
                    "DELETE FROM software_group_members WHERE group_id = ?",
                    params![num_id],
                )?;
                insert_group_members_and_revoke(tx, num_id, list)?;
            }
            Ok(())
        })
    }

    /// Deletes a software group, its memberships, and its group-scoped automations.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the delete transaction fails.
    pub fn delete_software_group(&self, group_id_raw: &str) -> AppResult<bool> {
        let Some(num_id) = parse_group_id(group_id_raw) else {
            return Ok(false);
        };
        let gid_str = num_id.to_string();
        let prefixed = format_group_id(num_id);
        let deleted = self.with_tx(|tx| {
            tx.execute(
                "DELETE FROM software_group_members WHERE group_id = ?",
                params![num_id],
            )?;
            tx.execute(
                "DELETE FROM automation_config WHERE scope_type = ? AND (scope_id = ? OR scope_id = ?)",
                params![SCOPE_GROUP, gid_str, prefixed],
            )?;
            let rows = tx.execute(
                "DELETE FROM software_groups WHERE group_id = ?",
                params![num_id],
            )?;
            Ok(rows > 0)
        })?;
        self.remove_lww_key(&build_group_lww_key(num_id));
        Ok(deleted)
    }

    /// Returns `Some((group_id, group_name))` if `ain` belongs to a virtual software group.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if querying `software_group_members` fails.
    pub fn get_group_for_device(&self, ain: &str) -> AppResult<Option<(i64, String)>> {
        let norm = normalize_ain(ain);
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT sg.group_id, sg.name FROM software_group_members sgm
                 JOIN software_groups sg ON sgm.group_id = sg.group_id
                 WHERE sgm.ain = ? OR replace(sgm.ain, ' ', '') = ? LIMIT 1",
                params![ain, norm],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(AppError::from)
        })
    }

    /// Verifies that no candidate AIN already belongs to another software group.
    fn validate_exclusive_members(
        &self,
        members: &[String],
        exclude_gid: Option<i64>,
    ) -> AppResult<()> {
        for ain in members {
            if let Some((existing_id, existing_name)) = self.get_group_for_device(ain)? {
                if Some(existing_id) != exclude_gid {
                    return Err(AppError::Conflict(format!(
                        "Device '{ain}' is already a member of group '{existing_name}' (id: {existing_id})"
                    )));
                }
            }
        }
        Ok(())
    }
}

// =============================================================================
/// Deduplicates and trims a list of member AIN strings while preserving insertion order.
fn sanitize_member_list(members: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(members.len());
    for raw in members {
        let trimmed = raw.trim();
        let norm = normalize_ain(trimmed);
        if !norm.is_empty() && seen.insert(norm) {
            out.push(trimmed.to_owned());
        }
    }
    out
}

// =============================================================================
/// Inserts `software_group_members` and revokes standalone `device`-scoped automations.
fn insert_group_members_and_revoke(
    conn: &Connection,
    group_id: i64,
    members: &[String],
) -> AppResult<()> {
    for ain in members {
        let norm = normalize_ain(ain);
        conn.execute(
            "INSERT INTO software_group_members (group_id, ain) VALUES (?, ?)",
            params![group_id, ain],
        )?;
        conn.execute(
            "DELETE FROM automation_config
             WHERE scope_type = ? AND (scope_id = ? OR replace(scope_id, ' ', '') = ?)",
            params![SCOPE_DEVICE, ain, norm],
        )?;
    }
    Ok(())
}
