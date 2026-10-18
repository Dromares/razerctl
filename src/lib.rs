//! razerctl — Linux rewrite of the Razer Synapse device-control core.
//! Shared by the `razerctl` CLI and the `razerctl-gui` desktop app.

pub mod battery;
pub mod device;
pub mod hidraw;
pub mod macros;
pub mod onboard;
pub mod protocol;
pub mod remap;
pub mod sha256;
pub mod units;
pub mod update;
