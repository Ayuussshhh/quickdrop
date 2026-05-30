//! OS integration layer (context menu, autostart helpers, etc.).
//!
//! Windows-specific code lives in `windows.rs` and is only compiled
//! on `target_os = "windows"`. Other platforms get stubs for now.

#[cfg(target_os = "windows")]
pub mod windows;
