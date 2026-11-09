//! Engine-independent walking-pad telemetry and opt-in bounded speed control.
//!
//! BLE is an optional feature. Protocol and recording tools also work offline.
//! No start/stop API; speed control requires fresh running-belt telemetry.

#[cfg(feature = "ble")]
pub mod ble;
pub mod control;
pub mod device;
pub mod kingsmith;
pub mod protocol;
pub mod recording;
pub mod telemetry;
