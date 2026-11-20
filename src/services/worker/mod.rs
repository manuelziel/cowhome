//! Background FritzBox polling worker and Digital Twin synchronization (`Layer 4`).
//!
//! Exposes [`FritzPollingWorker`] and [`PollingWorkerDeps`] alongside pure/stateful
//! builders and synchronization helpers.

mod maintenance;
pub mod poller;
pub mod state_builder;
pub mod sync_logic;

pub use poller::{FritzPollingWorker, PollingWorkerDeps};
