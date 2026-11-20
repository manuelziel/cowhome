//! Library root exposing CowHome's strict bottom-up Directed Acyclic Graph (DAG).
//!
//! Dependency flow:
//! `core` -> `models` -> `db` -> `services` / `setup` -> `web` / `cli` -> `main`.

pub mod cli;
pub mod core;
pub mod db;
pub mod models;
pub mod services;
pub mod setup;
pub mod web;
