//! Domain service, hardware gateway, weather, polling worker, & diagnostics layer (`Layer 4`).
//!
//! Depends strictly on `crate::core`, `crate::models`, and `crate::db`.

pub mod application;
pub mod automation;
pub mod diagnostics;
pub mod fritz;
pub mod rate_limiter;
pub mod weather;
pub mod worker;
