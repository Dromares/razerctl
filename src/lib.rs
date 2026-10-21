//! razerctl — Linux rewrite of the Razer Synapse device-control core.
//! Shared by the `razerctl` CLI and the `razerctl-gui` desktop app.

pub mod battery;
pub mod cli;
pub mod device;
pub mod firmware;
pub mod hidraw;
pub mod macros;
pub mod onboard;
pub mod probe;
pub mod protocol;
pub mod remap;
pub mod sha256;
pub mod units;
pub mod update;
