//! Direct live FritzBox `TR-064` & `AHA` connection test (`cowhome test`).
//!
//! Verifies FritzBox credentials, SOAP router management (`DeviceInfo` & `SystemStatus`),
//! and Smart Home `AHA` session login (`PBKDF2-HMAC-SHA256` / `MD5`) directly from
//! `config.toml` without requiring the background daemon to be running.

use std::path::Path;

use crate::cli::output::ConsolePrinter;
use crate::core::config::AppConfig;
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::services::fritz::{FritzAhaClient, FritzTr064Client};

// =============================================================================
/// Executes live `TR-064` and `AHA` connectivity probes against `config.fritzbox`.
///
/// # Errors
/// Returns [`AppError`] if FritzBox password is missing or either subsystem probe fails.
pub fn run_fritz_connection_test(
    config: &AppConfig,
    config_path: &Path,
    logger: &Logger,
    printer: ConsolePrinter,
) -> AppResult<()> {
    if config.fritzbox.password.trim().is_empty() {
        return Err(AppError::Config(format!(
            "No FritzBox password configured in '{}'",
            config_path.display()
        )));
    }

    printer.print_header(&format!(
        "Testing FritzBox Connection ({})",
        config.fritzbox.host
    ));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| AppError::Service(format!("Failed initializing tokio runtime: {err}")))?;

    let (tr064_ok, aha_ok) = rt.block_on(async {
        let tr_ok = test_tr064_subsystem(config, logger, printer).await;
        eprintln!();
        let ah_ok = test_aha_subsystem(config, logger, printer).await;
        eprintln!();
        (tr_ok, ah_ok)
    });

    if tr064_ok && aha_ok {
        printer.print_success("All connection tests passed! The credentials are valid.");
        Ok(())
    } else {
        Err(AppError::Auth(
            "Connection test failed. Please check your credentials in config.toml.".into(),
        ))
    }
}

// =============================================================================
/// Probes `TR-064` SOAP router management (`get_device_info` + `get_system_status`).
async fn test_tr064_subsystem(
    config: &AppConfig,
    logger: &Logger,
    printer: ConsolePrinter,
) -> bool {
    eprintln!("Testing TR-064 (Router Management)...");
    let client = match FritzTr064Client::new(
        &config.fritzbox.host,
        &config.fritzbox.username,
        &config.fritzbox.password,
        logger.clone(),
    ) {
        Ok(c) => c,
        Err(err) => {
            printer.print_error(&format!("TR-064 client initialization failed: {err}"));
            return false;
        }
    };
    let info_opt = client.get_device_info().await;
    match client.get_system_status().await {
        Ok(status) => {
            printer.print_success("TR-064 connection OK");
            let model = info_opt.as_ref().map_or("Unknown", |i| i.model.as_str());
            let fw = info_opt
                .as_ref()
                .map_or("Unknown", |i| i.software_version.as_str());
            eprintln!("  Model:    {model}");
            eprintln!("  Firmware: {fw}");
            eprintln!("  Uptime:   {}", format_human_uptime(status.uptime));
            true
        }
        Err(err) => {
            if let Some(info) = info_opt {
                printer.print_success("TR-064 DeviceInfo OK (WAN status restricted)");
                eprintln!("  Model:    {}", info.model);
                eprintln!("  Firmware: {}", info.software_version);
                return true;
            }
            printer.print_error("TR-064 connection FAILED");
            eprintln!("  Error: {err}");
            false
        }
    }
}

// =============================================================================
/// Probes `AHA` HTTP Smart Home authentication and `getdevicelistinfos` XML parsing.
async fn test_aha_subsystem(config: &AppConfig, logger: &Logger, printer: ConsolePrinter) -> bool {
    eprintln!("Testing AHA (Smart Home)...");
    let client = match FritzAhaClient::new(
        &config.fritzbox.host,
        &config.fritzbox.username,
        &config.fritzbox.password,
        logger.clone(),
    ) {
        Ok(c) => c,
        Err(err) => {
            printer.print_error(&format!("AHA client initialization failed: {err}"));
            return false;
        }
    };
    match client.get_devices().await {
        Ok(devices) => {
            printer.print_success("AHA connection OK");
            eprintln!("  Found {} Smart Home device(s).", devices.devices.len());
            true
        }
        Err(err) => {
            printer.print_error("AHA connection FAILED");
            eprintln!("  Error: {err}");
            false
        }
    }
}

// =============================================================================
/// Formats uptime in seconds as compact human text (`"14d 6h 12m"` or `"4m 18s"`).
fn format_human_uptime(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let mins = (total_secs % 3_600) / 60;
    let secs = total_secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}
