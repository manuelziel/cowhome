//! Pure-Rust FritzBox Smart Home (`AHA-HTTP`) and Router (`TR-064`) gateway clients.
//!
//! Replaces `pyfritzhome` and `fritzconnection` completely with statically compiled,
//! zero-C-dependency Rust implementations over `reqwest` (`rustls`) and `roxmltree`.

pub mod aha;
pub mod aha_xml;
pub mod auth;
pub mod tr064;
pub mod tr064_actions;
mod tr064_soap;
pub mod tr064_telephony;

pub use aha::{encode_hkr_tsoll, DeviceAttentionIssue, FritzAhaClient};
pub use aha_xml::{parse_device_list_xml, ParsedDeviceList, RawAhaDevice};
pub use tr064::{ConnectionStatus, FritzTr064Client, RouterInfo};
pub use tr064_actions::{GuestWlanStatus, HostEntry, WlanStatus};
pub use tr064_telephony::{AnsweringMachineStatus, CallEntry};
