# CowHome (`cowhome`)

An autonomous, statically linkable **Rust single-binary Smart Home Hub** for the FritzBox ecosystem (`AHA` & `TR-064`).

CowHome acts as a local **Single Source of Truth (Digital Twin)** in RAM and SQLite, providing a microsecond-latency, `RFC 6750`-hardened REST API for mobile applications (`home.libracow.com`) with zero external cloud or runtime dependencies.

Designed around a **3-file zero-bloat delivery model**: a release bundle consists exclusively of the compiled static **`cowhome`** binary (`musl`), a minimal privilege wrapper **`setup.sh`**, and **`README.md`**. All system templates (`config.toml`, `cowhome.service`, `openapi.yaml`, and Swagger UI) as well as the FHS 3.0 installer, updater, `systemd` controller, diagnostics engine, and interactive TUI live inside the single binary.

---

## Key Features

- **True Single-Binary Architecture:** Zero runtime dependency on Python, virtual environments, `glibc`, `libsystemd`, or OpenSSL (`x86_64-unknown-linux-musl` & `aarch64-unknown-linux-musl` for Raspberry Pi 5).
- **Digital Twin & 100% Backend-Autonomous Logic:** FritzBox serves strictly as a DECT-ULE/HAN-FUN radio gateway. Software room groups, custom names, comfort/eco presets, 7-day/48-slot weekly schedules (`HeatingSchedule` / `PowerSchedule` in home IANA timezone via `chrono-tz`), room sensor sync (`ApparentTemperature`), and standalone calibration offsets (`set_offset`) run 100% autonomously in CowHome.
- **Building Physics & Predictive Heating:** Implements `DIN 4108-2` mold/thermal-bridge protection (`HumidityControl` v2, $f_{\text{Rsi}} = 0.70$), `DIN EN 12098-1` predictive `AdaptiveStart` (`Pre-Heat`), `VDI 2067/3807` 4-factor thermal efficiency & 5-day HDD heating outlook, and `DIN EN IEC 62053-21` / `EnWG § 40` Smart Energy cost forecasting (`FRITZ!Smart Energy 200/210/250`).
- **Flash-Friendly Time-Series Engine:** SQLite `WAL` persistence with deadband compression, 30-minute heartbeats, pre-edge step-before anchors, zero-write upserts, and daily `00:00` retention auto-pruning.
- **Hardened Remote Access & Universal Diagnostics v1.0:** Header-only `RFC 6750` Bearer/`X-API-Key` auth (`subtle::ConstantTimeEq`), anti-spoofing `trusted_proxies`, 2-tier sliding-window `RateLimiter` (`120 req/min` + `5` failed logins/hour), `O_NOFOLLOW` atomic file I/O (`CWE-59`), native `/run/systemd/journal/socket` logging with secret redaction, and `GET /api/v1/system/diagnostics`.

---

## Applied Standards & Specifications

| Domain | Standards | Scope in CowHome |
| :--- | :--- | :--- |
| Heating Control | `DIN EN 12098-1`, `DIN EN 15232` | Automatic `Pre-Heat` & 5-tier priority |
| Moisture Guard | `DIN 4108-2`, `WMO-No. 8` | Magnus dew point & $f_{\text{Rsi}} = 0.70$ |
| Thermal & HDD | `VDI 2067`, `VDI 3807`, `ISO 15927-6` | 4-factor efficiency & 5-day HDD outlook |
| Smart Energy | `DIN EN IEC 62053-21`, `EnWG § 40` | Monotonic Wh/kWh metering & base fee |
| Radio & Router | `ETSI TS 102 939`, `TR-064` | DECT ULE / AHA XML & SOAP CPE telemetry |
| System & Init | `FHS 3.0`, `POSIX.1-2017`, `systemd(5)` | FHS paths, signals & unit sandboxing |
| App Security | `OWASP ASVS v4.0`, `RFC 6750`, `CWE-59` | Header auth, `O_NOFOLLOW` & redaction |
| Auth & Crypto | `NIST SP 800-63B`, `RFC 2617/7616` | Constant-time eq, PBKDF2 SID & Digest |
| REST & Health | `OpenAPI 3.0.3`, `IETF Health`, `ISA-18.2` | Schema contract, 5-block health & alerts |
| Persistence | `SQLite WAL`, `ACID`, `TOML v1.0.0` | Wear-leveled WAL & atomic config |
| Governance | `SemVer 2.0.0`, `RFC 1574`, `SPDX MIT` | `CHANGELOG.md` SSoT, DAG & Rustdoc |

---

## Documentation Index

- **[OpenAPI 3.0.3 Specification](assets/openapi.yaml):** Embedded Single Source of Truth served at `/api/v1/openapi.yaml` and `/api/v1/docs`.
- **[Project Specification](docs/specifications.md):** Architectural overview of the Digital Twin, time-series history sync, and diagnostics.
- **[API Capabilities](docs/api_capabilities.md):** Device capability matrix (`thermostat`, `sensor`, `switch`, `router`) and supported actions.
- **[Client Integration Guide](docs/client_integration.md):** Header auth, short polling, rate limiting, and Last-Write-Wins (LWW) offline sync.
- **[Automation & Rules Engine](docs/automation_rules.md):** Parameterized Smart Features, physical formulas, and 5-tier priority pyramid.

---

## Quick Start

```bash
# Check formatting, clippy (-D warnings), and shell syntax
./build.sh check

# Build static musl binaries (x86_64 & Raspberry Pi 5 ARM64)
./build.sh

# Install locally, verify FritzBox credentials, and start daemon
sudo ./setup.sh install
sudo cowhome config
sudo cowhome test
sudo systemctl enable --now cowhome.service
cowhome status
```

---

## License

Released under the [MIT License](LICENSE). Copyright (c) 2026 Manuel Ziel.
