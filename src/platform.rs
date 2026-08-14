//! Platform-specific service integration.
//!
//! Everything OS-specific lives in a whole-module cfg: `windows` holds the
//! Windows SCM/UAC implementation, `other` are the portable stubs for
//! platforms without service support yet (Linux systemd is planned).

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub use other::*;
