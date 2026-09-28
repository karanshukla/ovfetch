//! What this machine's Intel NPU needs from OpenVINO, and fetching a build that fits.
//!
//! Without the default `cli` feature only the offline half is built:
//! [`detect::machine`] reads sysfs and `ldconfig`, and [`status::status`]
//! checks it against the compiled-in data. No network, no downloads.

pub mod data;
pub mod detect;
pub mod status;
pub mod version;

#[cfg(feature = "cli")]
pub mod ci;
#[cfg(feature = "cli")]
pub mod consensus;
#[cfg(feature = "cli")]
pub mod install;
#[cfg(feature = "cli")]
pub mod net;
#[cfg(feature = "cli")]
pub mod resolve;
#[cfg(feature = "cli")]
pub mod sources;
