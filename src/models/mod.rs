//! Layer 2 Domain Models & Digital Twin (`device`, `group`, `weather`, `automation`, `registry`).
//!
//! Depends strictly downward on [`crate::core`] with zero upward dependencies to I/O,
//! SQLite, HTTP clients, `axum`, or CLI modules.

pub mod automation;
pub mod device;
pub mod group;
pub mod registry;
pub mod weather;
