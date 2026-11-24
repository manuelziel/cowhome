# Changelog

All notable changes to **CowHome** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.2] - 2026-11-24

### Added
- Post-save `systemctl reload` (`SIGHUP`) / `systemctl restart` prompt and `[y/N]`-guarded `Reset to Defaults` in `src/cli/config_menu.rs`.

### Changed
- Upgraded crate to Rust Edition `2024` (`rust-version = "1.100"`) and synchronized `--range` CLI help & `openapi.yaml` `requested_range` enum across all 8 canonical windows (`24h`..`all`).
- Standardized `Runtime` (`7` items without `Setup / Maintenance`), `Setup` (`1`-word labels), and `Configure` TUIs (`runtime_menu.rs`, `setup_menu.rs`, `config_menu.rs`, `config_sections.rs`) with dynamic `Save Changes` and context-sensitive `0) Back` vs. `0) Exit`.

## [0.0.1] - 2026-11-19

### Added
- Single-binary Rust core (`src/core/`, `src/models/`) with FHS 3.0 paths, `CWE-59` security, `SIGHUP` config, journald logger, DIN 4108-2/VDI 2067/EnWG § 40 physics, and `StateRegistry`.
- Wear-leveled `WAL` SQLite engine (`src/db/`) with zero-write/LWW caches, Deadband + Pre-Edge Anchor history, Single-Authority groups/automations, 4-factor efficiency, and ramp analytics.
- Pure-Rust FritzBox AHA (`PBKDF2`/`MD5`, HAN-FUN linking) & TR-064 SOAP Digest gateways, Open-Meteo `WeatherService`, async `FritzPollingWorker`, and 7-rule 5-tier `RulesEngine`.
- `axum` REST API v1 (`/api/v1/*`, RFC 6750 auth, 2-tier `RateLimiter`, LWW, deferred room-sensor valve actuation, embedded OpenAPI/Swagger UI) and 5-block Universal Diagnostics Schema v1.0.
- POSIX CLI, FHS `systemd` installer (`src/setup/`), and dynamic list-generated TUI menus (`print_action_menu`, `resolve_menu_action`) with `sudo` elevation and `JSON`/`YAML`/`TOML` backups.
