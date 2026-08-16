//! Platform-specific service integration.
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod openrc;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(any(windows, target_os = "linux")))]
mod other;
#[cfg(not(any(windows, target_os = "linux")))]
pub use other::*;
