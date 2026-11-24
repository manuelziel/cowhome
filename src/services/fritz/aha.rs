//! Pure-Rust FritzBox Smart Home (`AHA-HTTP`) async client (`homeautoswitch.lua`).
//!
//! Manages session authentication (`SID`) with automatic transparent re-login on
//! session expiry (`HTTP 403` / `0000000000000000`), exposes `getdevicelistinfos`
//! XML telemetry parsing, and dispatches actuator commands for thermostats, switches,
//! roller blinds, and color/Kelvin smart bulbs.

use std::sync::RwLock;
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;

use super::aha_xml::{ParsedDeviceList, RawAhaDevice, parse_device_list_xml};
use super::auth::{build_aha_base_url, is_valid_sid, login_aha_sid, percent_encode_component};
use crate::core::constants::service_defaults::DEFAULT_FRITZBOX_TIMEOUT_SECS;
use crate::core::domain_constants::hardware::{
    blind_states,
    fritz_api::{
        AHA_ENDPOINT_PATH, CMD_GET_DEVICE_LIST, CMD_SET_BLIND, CMD_SET_COLOR, CMD_SET_COLOR_TEMP,
        CMD_SET_HKR_TSOLL, CMD_SET_LEVEL_PERCENTAGE, CMD_SET_SWITCH_OFF, CMD_SET_SWITCH_ON,
        CMD_SET_SWITCH_TOGGLE, CMD_SET_UNMAPPED_COLOR, DEFAULT_BATTERY_LEVEL, HALF_DEGREE_FACTOR,
        MAX_COLOR_HUE, MAX_COLOR_TEMP_KELVIN, MAX_LEVEL_PERCENT, MAX_THERMOSTAT_TEMP,
        MIN_COLOR_TEMP_KELVIN, MIN_THERMOSTAT_TEMP, TSOLL_OFF_RAW, TSOLL_ON_RAW,
    },
};
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::time_util::normalize_ain;
use crate::core::validation::sanitize_log_uri;

// =============================================================================
/// Runtime credentials and base URL for FritzBox AHA requests.
#[derive(Debug, Clone)]
struct AhaEndpointConfig {
    /// Normalized HTTP(S) base URL (`"http://fritz.box"`).
    base_url: String,
    /// FritzBox username.
    username: String,
    /// FritzBox password.
    password: String,
}

// =============================================================================
/// Device health attention entry returned by [`FritzAhaClient::get_low_battery_devices`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceAttentionIssue {
    /// Device AIN.
    pub ain: String,
    /// Display name reported by the FritzBox.
    pub name: String,
    /// Human-readable issue label (`"Low Battery"` or `"Error Code N"`).
    pub issue: String,
    /// Reported battery percentage (`0`..=`100`).
    pub battery_level: u8,
}

// =============================================================================
/// Async Pure-Rust FritzBox AHA client safe for concurrent sharing via `Arc`.
#[derive(Debug)]
pub struct FritzAhaClient {
    /// Pooled `rustls` HTTP client.
    http: Client,
    /// Hot-reloadable FritzBox host and credentials.
    endpoint: RwLock<AhaEndpointConfig>,
    /// Active 16-hex-character FritzBox session ID (`SID`), guarded to serialize logins.
    sid: AsyncMutex<Option<String>>,
    /// Application logger with secret redaction.
    logger: Logger,
}

impl FritzAhaClient {
    /// Constructs a new [`FritzAhaClient`] for `host`, `username`, and `password`.
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if initializing the TLS/HTTP client fails.
    pub fn new(host: &str, username: &str, password: &str, logger: Logger) -> AppResult<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_FRITZBOX_TIMEOUT_SECS))
            .build()
            .map_err(|e| AppError::Service(format!("Failed building AHA HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoint: RwLock::new(AhaEndpointConfig {
                base_url: build_aha_base_url(host),
                username: username.to_owned(),
                password: password.to_owned(),
            }),
            sid: AsyncMutex::new(None),
            logger,
        })
    }

    /// Updates FritzBox host/credentials on `SIGHUP` reload and invalidates stale SIDs.
    pub async fn reconfigure(&self, host: &str, username: &str, password: &str) {
        let new_base = build_aha_base_url(host);
        let mut changed = false;
        if let Ok(mut cfg) = self.endpoint.write() {
            changed =
                cfg.base_url != new_base || cfg.username != username || cfg.password != password;
            cfg.base_url = new_base;
            cfg.username = username.to_owned();
            cfg.password = password.to_owned();
        }
        if changed {
            let mut guard = self.sid.lock().await;
            *guard = None;
        }
    }

    /// Forces a fresh AHA session handshake and caches the obtained `SID`.
    ///
    /// # Errors
    /// Returns [`AppError::Auth`] on invalid credentials or [`AppError::Service`] on network error.
    pub async fn connect(&self) -> AppResult<()> {
        let mut guard = self.sid.lock().await;
        let cfg = self.read_endpoint()?;
        let new_sid =
            login_aha_sid(&self.http, &cfg.base_url, &cfg.username, &cfg.password).await?;
        self.logger
            .debug(&format!("Connected to FritzBox AHA on {}", cfg.base_url));
        *guard = Some(new_sid);
        Ok(())
    }

    /// Fetches and parses all physical Smart Home devices (`getdevicelistinfos`).
    ///
    /// # Errors
    /// Returns [`AppError`] on authentication, HTTP, or XML parsing failure.
    pub async fn get_devices(&self) -> AppResult<ParsedDeviceList> {
        let xml = self.execute_cmd(CMD_GET_DEVICE_LIST, None, &[]).await?;
        parse_device_list_xml(&xml)
    }

    /// Looks up a single physical Smart Home device by exact or space-normalized AIN.
    ///
    /// # Errors
    /// Returns [`AppError::NotFound`] if the AIN is absent or [`AppError`] on gateway failure.
    pub async fn get_device(&self, ain: &str) -> AppResult<RawAhaDevice> {
        let target = normalize_ain(ain);
        let list = self.get_devices().await?;
        list.devices
            .into_iter()
            .find(|d| d.normalized_ain == target)
            .ok_or_else(|| AppError::NotFound(format!("AHA device '{ain}' not found")))
    }

    /// Turns a smart switch explicitly ON (`true`) or OFF (`false`).
    ///
    /// # Errors
    /// Returns [`AppError`] if the AHA switch command fails.
    pub async fn set_switch(&self, ain: &str, state: bool) -> AppResult<()> {
        let cmd = if state {
            CMD_SET_SWITCH_ON
        } else {
            CMD_SET_SWITCH_OFF
        };
        self.execute_cmd(cmd, Some(ain), &[]).await?;
        self.logger.info(&format!(
            "Turned switch {ain} {}",
            if state { "ON" } else { "OFF" }
        ));
        Ok(())
    }

    /// Toggles the relay state of a smart switch (`setswitchtoggle`).
    ///
    /// # Errors
    /// Returns [`AppError`] if the AHA toggle command fails.
    pub async fn toggle_switch(&self, ain: &str) -> AppResult<()> {
        self.execute_cmd(CMD_SET_SWITCH_TOGGLE, Some(ain), &[])
            .await?;
        self.logger.info(&format!("Toggled switch {ain}"));
        Ok(())
    }

    /// Sets the radiator valve target temperature (`0.0` = OFF/`253`, `8.0..=28.0 °C` in `0.5 °C`).
    ///
    /// # Errors
    /// Returns [`AppError`] if `sethkrtsoll` fails.
    pub async fn set_target_temperature(&self, ain: &str, temperature: f64) -> AppResult<()> {
        let raw_param = encode_hkr_tsoll(temperature).to_string();
        self.execute_cmd(CMD_SET_HKR_TSOLL, Some(ain), &[("param", &raw_param)])
            .await?;
        self.logger.info(&format!(
            "Set target temperature for {ain} to {temperature}°C"
        ));
        Ok(())
    }

    /// Applies CowHome's backend comfort temperature preset to `ain`.
    ///
    /// # Errors
    /// Returns [`AppError`] if `sethkrtsoll` fails.
    pub async fn set_thermostat_comfort(&self, ain: &str, temperature: f64) -> AppResult<()> {
        self.set_target_temperature(ain, temperature).await?;
        self.logger.info(&format!(
            "Set {ain} to backend comfort temperature ({temperature}°C)"
        ));
        Ok(())
    }

    /// Applies CowHome's backend eco setback temperature preset to `ain`.
    ///
    /// # Errors
    /// Returns [`AppError`] if `sethkrtsoll` fails.
    pub async fn set_thermostat_eco(&self, ain: &str, temperature: f64) -> AppResult<()> {
        self.set_target_temperature(ain, temperature).await?;
        self.logger.info(&format!(
            "Set {ain} to backend eco temperature ({temperature}°C)"
        ));
        Ok(())
    }

    /// Controls a DECT-ULE roller blind (`"open"`, `"close"`, or `"stop"`).
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] on invalid target state or [`AppError`] on gateway failure.
    pub async fn set_blind_state(&self, ain: &str, state: &str) -> AppResult<()> {
        if !blind_states::ALL.contains(&state) {
            return Err(AppError::Validation(format!(
                "Invalid blind state: {state}"
            )));
        }
        self.execute_cmd(CMD_SET_BLIND, Some(ain), &[("target", state)])
            .await?;
        self.logger.info(&format!("Set blind {ain} to {state}"));
        Ok(())
    }

    /// Sets the percentage level (`0`..=`100`) of a dimmable bulb or roller blind.
    ///
    /// # Errors
    /// Returns [`AppError`] if `setlevelpercentage` fails.
    pub async fn set_level(&self, ain: &str, level: u8) -> AppResult<()> {
        let clamped = level.min(MAX_LEVEL_PERCENT).to_string();
        self.execute_cmd(CMD_SET_LEVEL_PERCENTAGE, Some(ain), &[("level", &clamped)])
            .await?;
        self.logger
            .info(&format!("Set level for {ain} to {clamped}%"));
        Ok(())
    }

    /// Sets HSV `hue` (`0`..=`359`) and `saturation` (`0`..=`255`) on a smart bulb (`DECT 500`).
    ///
    /// # Errors
    /// Returns [`AppError`] if both `setunmappedcolor` and fallback `setcolor` fail.
    pub async fn set_color(&self, ain: &str, hue: u16, saturation: u8) -> AppResult<()> {
        let hue_str = hue.min(MAX_COLOR_HUE).to_string();
        let sat_str = saturation.to_string();
        let params = [
            ("hue", hue_str.as_str()),
            ("saturation", sat_str.as_str()),
            ("duration", "0"),
        ];
        if self
            .execute_cmd(CMD_SET_UNMAPPED_COLOR, Some(ain), &params)
            .await
            .is_err()
        {
            self.execute_cmd(CMD_SET_COLOR, Some(ain), &params).await?;
        }
        self.logger.info(&format!(
            "Set color for {ain} (Hue: {hue_str}, Sat: {sat_str})"
        ));
        Ok(())
    }

    /// Sets white-light color temperature in Kelvin (`2700`..=`6500 K`) on a smart bulb.
    ///
    /// # Errors
    /// Returns [`AppError`] if `setcolortemperature` fails.
    pub async fn set_color_temperature(&self, ain: &str, kelvin: u16) -> AppResult<()> {
        let k_str = kelvin
            .clamp(MIN_COLOR_TEMP_KELVIN, MAX_COLOR_TEMP_KELVIN)
            .to_string();
        self.execute_cmd(
            CMD_SET_COLOR_TEMP,
            Some(ain),
            &[("temperature", &k_str), ("duration", "0")],
        )
        .await?;
        self.logger
            .info(&format!("Set color temperature for {ain} to {k_str}K"));
        Ok(())
    }

    /// Returns all physical devices with low battery (`< threshold`) or non-zero valve errors.
    pub async fn get_low_battery_devices(&self, threshold: u8) -> Vec<DeviceAttentionIssue> {
        let list = match self.get_devices().await {
            Ok(res) => res,
            Err(err) => {
                self.logger
                    .warning(&format!("Failed to run AHA health check: {err}"));
                return Vec::new();
            }
        };
        let mut issues = Vec::new();
        for dev in list.devices {
            let bat = dev.battery_level.unwrap_or(DEFAULT_BATTERY_LEVEL);
            if dev.battery_low || bat < threshold {
                issues.push(DeviceAttentionIssue {
                    ain: dev.ain,
                    name: dev.name,
                    issue: String::from("Low Battery"),
                    battery_level: bat,
                });
            } else if dev.error_code != 0 {
                issues.push(DeviceAttentionIssue {
                    ain: dev.ain,
                    name: dev.name,
                    issue: format!("Error Code {}", dev.error_code),
                    battery_level: bat,
                });
            }
        }
        issues
    }

    /// Dispatches a `homeautoswitch.lua` command, transparently re-authenticating once on expired SID.
    ///
    /// # Errors
    /// Returns [`AppError`] on login failure, HTTP error, or network timeout.
    async fn execute_cmd(
        &self,
        switchcmd: &str,
        ain: Option<&str>,
        extra: &[(&str, &str)],
    ) -> AppResult<String> {
        let sid = self.ensure_sid(false).await?;
        match self.send_switchcmd(&sid, switchcmd, ain, extra).await {
            Ok(body) => Ok(body),
            Err(AppError::Auth(_)) => {
                let fresh_sid = self.ensure_sid(true).await?;
                self.send_switchcmd(&fresh_sid, switchcmd, ain, extra).await
            }
            Err(err) => Err(err),
        }
    }

    /// Returns the cached `SID`, or performs a fresh login when `force_refresh` or `None`.
    ///
    /// # Errors
    /// Returns [`AppError`] if acquiring a session ID fails.
    async fn ensure_sid(&self, force_refresh: bool) -> AppResult<String> {
        let mut guard = self.sid.lock().await;
        if !force_refresh
            && let Some(ref existing) = *guard
            && is_valid_sid(existing)
        {
            return Ok(existing.clone());
        }
        let cfg = self.read_endpoint()?;
        let fresh = login_aha_sid(&self.http, &cfg.base_url, &cfg.username, &cfg.password).await?;
        *guard = Some(fresh.clone());
        Ok(fresh)
    }

    /// Sends a single HTTP `GET` to `/webservices/homeautoswitch.lua`.
    ///
    /// # Errors
    /// Returns [`AppError::Auth`] on expired SID (`401`/`403`) or [`AppError::Service`] on failure.
    async fn send_switchcmd(
        &self,
        sid: &str,
        switchcmd: &str,
        ain: Option<&str>,
        extra: &[(&str, &str)],
    ) -> AppResult<String> {
        let cfg = self.read_endpoint()?;
        let mut url = format!(
            "{}{AHA_ENDPOINT_PATH}?sid={}&switchcmd={}",
            cfg.base_url,
            percent_encode_component(sid),
            percent_encode_component(switchcmd)
        );
        if let Some(raw_ain) = ain {
            url.push_str(&format!(
                "&ain={}",
                percent_encode_component(&normalize_ain(raw_ain))
            ));
        }
        for &(k, v) in extra {
            url.push_str(&format!(
                "&{}={}",
                percent_encode_component(k),
                percent_encode_component(v)
            ));
        }
        let resp = self.http.get(&url).send().await.map_err(|e| {
            let safe = sanitize_log_uri(&e.to_string());
            AppError::Service(format!("AHA '{switchcmd}' request error: {safe}"))
        })?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(AppError::Auth(format!(
                "AHA session expired (HTTP {status})"
            )));
        }
        if !resp.status().is_success() {
            return Err(AppError::Service(format!(
                "AHA '{switchcmd}' failed with HTTP {status}"
            )));
        }
        let text = resp.text().await.map_err(|e| {
            let safe = sanitize_log_uri(&e.to_string());
            AppError::Service(format!("AHA '{switchcmd}' decode error: {safe}"))
        })?;
        if text.trim() == "0000000000000000" {
            return Err(AppError::Auth("AHA returned invalid zero SID".into()));
        }
        Ok(text)
    }

    /// Reads a cloned snapshot of [`AhaEndpointConfig`].
    ///
    /// # Errors
    /// Returns [`AppError::Service`] if the configuration `RwLock` is poisoned.
    fn read_endpoint(&self) -> AppResult<AhaEndpointConfig> {
        self.endpoint
            .read()
            .map(|g| g.clone())
            .map_err(|_| AppError::Service("AHA endpoint lock poisoned".into()))
    }
}

// =============================================================================
/// Encodes a target temperature (`°C`) into AVM's raw `0.5 °C` `sethkrtsoll` parameter.
#[must_use]
pub fn encode_hkr_tsoll(temperature: f64) -> u16 {
    if temperature <= 0.0 {
        TSOLL_OFF_RAW
    } else if temperature > MAX_THERMOSTAT_TEMP {
        TSOLL_ON_RAW
    } else {
        let clamped = temperature.clamp(MIN_THERMOSTAT_TEMP, MAX_THERMOSTAT_TEMP);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (clamped * HALF_DEGREE_FACTOR).round() as u16;
        steps
    }
}
