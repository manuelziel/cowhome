//! Integrated FHS 3.0 system installer, user provisioner, and `systemd` manager (Layer 3).
//!
//! Replaces external Bash setup libraries (`lib/setup/*.sh`) so the compiled single
//! binary executes `install`, `update`, `reinstall`, `uninstall`, `purge`, and `status`
//! directly while decoupled from terminal formatting via [`SetupEvent`].

pub mod deps;
pub mod installer;
pub mod status;
pub mod systemd;
pub mod users;

// =============================================================================
/// UI-agnostic progress events emitted by setup workflows to CLI observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupEvent {
    /// Informational progress step (`ℹ ...`).
    Step(String),
    /// Successfully completed provisioning or teardown action (`✓ ...`).
    Completed(String),
    /// Intentionally skipped step such as preserved `config.toml` (`ℹ ...`).
    Skipped(String),
    /// Non-fatal setup warning (`✗ ...`).
    Warning(String),
}
