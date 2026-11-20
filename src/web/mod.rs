//! HTTP REST API v1 (`axum`), `RFC 6750` header authentication, & daemon supervisor (`Layer 5`).
//!
//! Sits alongside `crate::cli` above `crate::services` in CowHome's strict bottom-up DAG.

pub mod daemon;
pub mod handlers;
pub mod middleware;
pub mod response;
pub mod router;
pub mod state;

pub use router::build_router;
pub use state::AppState;
