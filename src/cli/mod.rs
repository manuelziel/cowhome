//! Terminal user interface (TUI) and POSIX CLI routing layer (DAG Layer 6b).
//!
//! Sits atop `core`, `db`, `services`, and `setup` without cyclic back-references.

pub mod args;
pub mod backup_db;
pub mod config_menu;
pub mod config_prompts;
pub mod config_sections;
pub mod emergency_disable;
pub mod fritz_test;
pub mod import_export;
pub mod input;
pub mod output;
pub mod runtime_menu;
pub mod setup_args;
pub mod setup_menu;
pub mod setup_ops;
pub mod status_devices;
pub mod status_live;
pub mod status_view;
pub mod yaml_codec;
