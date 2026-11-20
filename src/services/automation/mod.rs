//! Smart Automation Engine (`RulesEngine`) & all 7 stateful building-physics rules.
//!
//! Modularizes CowHome's Single-Authority scope resolution, decision tracing, and
//! reactive/predictive climate rules (`HeatingSchedule`, `AdaptiveStart`,
//! `HumidityControl` v2, `ApparentTemperature`, `VacationMode`, `WeatherSmartHeating`,
//! and `PresenceProfiling`).

pub mod actuation;
mod adaptive_helpers;
pub mod adaptive_start;
pub mod apparent_temp;
mod chain;
mod chain_climate;
pub mod climate;
pub mod humidity;
mod humidity_fsm;
mod humidity_helpers;
pub mod presence;
pub mod runner;
pub mod schedule;
mod schedule_slots;
mod scope_binding;
pub mod scopes;
pub mod trace;

pub use runner::RulesEngine;
