//! Universal Diagnostics Schema v1.0 domain collector (`DiagnosticsService`).
//!
//! Combines `IETF` health checks, `ANSI/ISA-18.2` hardware/system alerts,
//! live or SQLite-reconstructed Digital Twin state, `RulesEngine` decision traces,
//! semantic curve summaries, and heating-ramp diagnostics.

pub mod alerts;
pub mod collector;
mod series;

pub use collector::DiagnosticsService;
