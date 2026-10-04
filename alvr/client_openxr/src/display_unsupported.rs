//! `DisplayConnection` on a platform this client does not target.
//!
//! The real implementation ([`crate::display`], on `unix`) opens a Wayland connection with
//! `dlopen`/`dlsym` and hands the `wl_display *` to the OpenXR session. Android gets the display
//! from its Java host; the Steam Frame, the only desktop target, is gamescope/kwin on Wayland.
//!
//! Windows and WASI are not build targets — but they are *workspace members'* targets, and a
//! `cargo check --workspace` (CI's own command) must not fail on a platform nobody ships. So this
//! file exists to keep the type nameable and to fail at runtime with a sentence instead of at build
//! time with a missing symbol. It is deliberately tiny: it has no `Wayland` variant, because there
//! is nothing here that could produce one.

use std::os::raw::c_void;

/// A display the session can bind to, or the reason there is none.
pub enum DisplayConnection {
    /// Always this, on a platform with no Wayland to open.
    Unavailable { reason: String },
}

impl DisplayConnection {
    /// There is no client-owned display on this platform, and the reason says so rather than
    /// pretending the environment is at fault.
    pub fn discover() -> Self {
        Self::Unavailable {
            reason: "this client binds a desktop OpenXR session through Wayland, which this \
                     platform does not provide; it builds here only so the workspace does"
                .to_owned(),
        }
    }

    /// No Wayland handle exists on this platform, so there is never one to return.
    pub fn as_wayland(&self) -> Option<*mut c_void> {
        None
    }

    /// The reason there is no display, if there is none.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Unavailable { reason } => Some(reason),
        }
    }
}
