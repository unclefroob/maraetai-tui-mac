//! Shared types between `maraetaid` (the daemon) and `maraetai` (the TUI):
//! config/credential loading, Subsonic auth-token generation, control-
//! channel naming/wire types, and runtime-path resolution. Keeping these
//! here means the two binaries cannot drift apart on how they authenticate
//! or where they find each other.
//!
//! The control channel itself is platform-specific — D-Bus on Linux
//! (`dbus`, its naming constants), a Unix-socket protocol on macOS
//! (`control_protocol`, its wire types) — since macOS has no D-Bus session
//! bus by default. `control_protocol` stays compiled on every platform
//! (it's plain data, and Linux's own D-Bus types reuse its tuple shapes),
//! but `dbus` is Linux-only since nothing else has any use for it.

pub mod auth;
pub mod config;
#[cfg(target_os = "linux")]
pub mod dbus;
pub mod control_protocol;
pub mod error;
pub mod paths;
pub mod spectrum;

pub use config::{Config, Credentials};
pub use error::{Error, Result};
