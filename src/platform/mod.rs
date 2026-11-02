//! What GPUI doesn't do for us, per platform.

#[cfg(target_os = "macos")]
pub mod drag_out;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod webview;
