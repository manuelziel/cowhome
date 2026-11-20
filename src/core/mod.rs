//! Core DAG root layer (`constants`, `domain_constants`, `error`, `validation`,
//! `http_security`, `time_util`, `process`, `security`, `logger`, `config`,
//! `config_io`, and `diagnostics`).
//!
//! Modules are ordered strictly bottom-up with zero upward dependencies to `services`,
//! `setup`, or `cli`.

pub mod config;
pub mod config_io;
pub mod config_schema;
pub mod constants;
pub mod diagnostics;
pub mod domain_constants;
pub mod energy_analytics;
pub mod error;
pub mod heating_forecast;
pub mod http_security;
pub mod logger;
pub mod process;
pub mod psychrometrics;
pub mod security;
pub mod time_util;
pub mod validation;
