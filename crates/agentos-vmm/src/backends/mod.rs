//! Per-OS hypervisor backends. Each module compiles only on its platform.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod cloud_hypervisor;

#[cfg(target_os = "windows")]
pub mod windows;
