//! The display connection a desktop OpenXR session binds through.
//!
//! On Android the runtime lives in the system and the display is Java's: `session_create_info`
//! passes the EGL display/config/context handles the host gave us, and the client owns nothing.
//! On a desktop — and the Steam Frame is gamescope/kwin on **Wayland** — *the client opens the
//! display itself*. `XR_KHR_opengl_enable`'s `GraphicsBindingOpenGLWaylandKHR` is a single
//! `wl_display *`; nobody opens one for us, and the runtime cannot create a session without it.
//!
//! # Why this is `dlopen` and not a dependency
//!
//! The aarch64 sysroot this client is cross-built with has **no Wayland libraries or headers**
//! (nor EGL, GL or Vulkan — `alvr_graphics` reaches those through `khronos-egl`'s dynamic loading
//! and `glow`/`wgpu` for the same reason). A `wayland-client` build dependency would compile here
//! and fail to link for the target. `wl_display_connect` is one symbol, so we resolve it at
//! runtime and hand the pointer through: a missing `libwayland-client` is then a startup
//! diagnostic instead of a build break.
//!
//! # The one thing not verified here
//!
//! The spec wants the `wl_display` in the binding to be the one the OpenGL context was created
//! on. Our EGL context is opened by `wgpu`'s GLES backend, which calls `eglGetPlatformDisplay`
//! with the default display — a *second* connection to the same compositor. Whether Valve's
//! runtime accepts that is a device-side question, and the reason this file is one small,
//! separately-testable unit rather than inlined into the entry point: if it is rejected, the
//! fix is to hand the same `wl_display` to EGL, and that is a change here only.

use std::ffi::CString;
use std::os::raw::c_void;

/// A display the session can bind to, or the reason there is none.
pub enum DisplayConnection {
    /// The client opened a Wayland connection and owns it for the process lifetime.
    #[cfg(not(target_os = "android"))]
    Wayland(WaylandDisplay),
    /// No client-owned connection. Correct on Android, where the host supplies the display.
    /// A startup failure everywhere else, and `reason` is what the operator needs to read.
    Unavailable { reason: String },
}

#[cfg(not(target_os = "android"))]
pub struct WaylandDisplay {
    /// The `wl_display *` passed to the runtime. Never null.
    display: *mut c_void,
    /// `libwayland-client`, held open for the process lifetime: the session and the compositor
    /// both keep pointers into it. Deliberately never `dlclose`d.
    _library: Library,
}

impl DisplayConnection {
    /// Android: the runtime supplies the display and the client owns no connection.
    #[cfg(target_os = "android")]
    pub fn discover() -> Self {
        Self::Unavailable {
            reason: "the Android host supplies the display".to_owned(),
        }
    }

    /// Desktop: take the Wayland display if there is one, otherwise say precisely why not.
    ///
    /// Never panics. Every failure is a diagnosis, because the alternative — `entry_point`
    /// aborting on an `unimplemented!()` — is what this file exists to replace, and it is
    /// indistinguishable from a crash in the field.
    #[cfg(not(target_os = "android"))]
    pub fn discover() -> Self {
        match WaylandDisplay::connect() {
            Ok(wayland) => Self::Wayland(wayland),
            Err(reason) => Self::Unavailable { reason },
        }
    }

    #[cfg(not(target_os = "android"))]
    pub fn as_wayland(&self) -> Option<*mut c_void> {
        match self {
            Self::Wayland(wayland) => Some(wayland.display),
            Self::Unavailable { .. } => None,
        }
    }

    /// The reason there is no display, if there is none.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            #[cfg(not(target_os = "android"))]
            Self::Wayland(_) => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

/// `dlopen`ed library handle. Intentionally has no `Drop`: see `WaylandDisplay::_library`.
///
/// The handle is never *read* — holding it is the whole point, because `dlclose` would unmap the
/// `wl_display_connect` the session is still using. A named field would not make that visible to
/// the compiler either; the comment is the contract.
#[cfg(not(target_os = "android"))]
struct Library(#[allow(dead_code)] *mut c_void);

#[cfg(not(target_os = "android"))]
type WlDisplayConnectFn = unsafe extern "C" fn(name: *const libc::c_char) -> *mut c_void;

#[cfg(not(target_os = "android"))]
impl WaylandDisplay {
    /// Wayland sonames, in order. `libwayland-client.so.0` is the ABI name the loader uses;
    /// the unversioned name is what a `-dev` package or a manual build leaves behind.
    const LIBRARIES: [&'static str; 2] = ["libwayland-client.so.0", "libwayland-client.so"];
    const CONNECT_SYMBOL: &'static str = "wl_display_connect";

    fn connect() -> Result<Self, String> {
        let (connect, library) = Library::open_with(&Self::LIBRARIES, Self::CONNECT_SYMBOL)
            .map_err(|e| {
                format!(
                    "no Wayland client library: {e}. The client binds its OpenXR session through \
                     a wl_display and cannot start without one; if this session is on X11 \
                     (XWayland), that path is not implemented yet."
                )
            })?;

        let connect: WlDisplayConnectFn = unsafe {
            // SAFETY: `open_with` resolved this exact symbol out of libwayland-client, whose
            // prototype is `wl_display *wl_display_connect(const char *name)`.
            std::mem::transmute(connect)
        };

        // A null name means "use WAYLAND_DISPLAY, or wayland-0", which is what a compositor-launched
        // app expects.
        let display = unsafe { connect(std::ptr::null()) };

        if display.is_null() {
            let socket = std::env::var("WAYLAND_DISPLAY").ok();
            return Err(format!(
                "wl_display_connect returned null — libwayland-client loaded but no compositor \
                 answered. WAYLAND_DISPLAY={socket:?}, XDG_RUNTIME_DIR={:?}",
                std::env::var("XDG_RUNTIME_DIR").ok()
            ));
        }

        Ok(Self {
            display,
            _library: library,
        })
    }
}

#[cfg(not(target_os = "android"))]
impl Library {
    /// Open the first of `names` that both loads and defines `symbol`, returning the symbol
    /// address and the handle that keeps it valid.
    fn open_with(names: &[&str], symbol: &str) -> Result<(*const c_void, Self), String> {
        let symbol_c = CString::new(symbol).map_err(|e| e.to_string())?;

        let mut last_error = String::from("no candidate library names were tried");

        for name in names {
            let Ok(name_c) = CString::new(*name) else {
                continue;
            };

            // SAFETY: `name_c` is a valid NUL-terminated C string for the duration of the call.
            let handle =
                unsafe { libc::dlopen(name_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                // `dlerror` already names the library it could not open; re-prefixing the name
                // would print it twice.
                last_error = dl_error();
                continue;
            }

            // SAFETY: `handle` came from `dlopen` and is still open.
            let address = unsafe { libc::dlsym(handle, symbol_c.as_ptr()) };
            if address.is_null() {
                last_error = format!("{name}: loaded but has no `{symbol}`");
                // This handle is ours and this attempt is over; the next candidate is a fresh
                // library with the same name in a different directory.
                unsafe { libc::dlclose(handle) };
                continue;
            }

            return Ok((address, Self(handle)));
        }

        Err(last_error)
    }
}

/// The dynamic loader's last error, or a placeholder if it has none to give.
#[cfg(not(target_os = "android"))]
fn dl_error() -> String {
    // SAFETY: `dlerror` returns either null or a pointer to a NUL-terminated string owned by the
    // loader; it is not freed and stays valid until the next loader call on this thread.
    let error = unsafe { libc::dlerror() };
    if error.is_null() {
        return "unknown dlopen failure".to_owned();
    }

    // SAFETY: non-null, and `dlerror` guarantees NUL termination.
    unsafe { std::ffi::CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    /// Unwrap the error half of `open_with`, whose success type deliberately holds a raw pointer
    /// handle and is therefore not `Debug`.
    fn expect_open_error(names: &[&str], symbol: &str) -> String {
        match Library::open_with(names, symbol) {
            Err(error) => error,
            Ok(_) => panic!("expected `{names:?}` to fail to resolve `{symbol}`"),
        }
    }

    #[test]
    fn a_missing_library_names_what_it_tried() {
        let error = expect_open_error(&["libdefinitely-not-installed.so.7"], "wl_display_connect");
        assert!(
            error.contains("libdefinitely-not-installed.so.7"),
            "the error must name the candidate it tried, got: {error}"
        );
    }

    #[test]
    fn a_library_without_the_symbol_is_rejected_rather_than_returned() {
        // libc is always present and never defines `wl_display_connect`.
        let error = expect_open_error(&["libc.so.6"], "wl_display_connect");
        assert!(
            error.contains("no `wl_display_connect`"),
            "the error must say the library loaded but lacked the symbol, got: {error}"
        );
    }

    #[test]
    fn discover_reports_a_reason_instead_of_panicking() {
        // Whatever the environment, `discover` must return a diagnosis: this container has no
        // compositor, a device does, and the same code path has to serve both.
        match DisplayConnection::discover() {
            #[cfg(not(target_os = "android"))]
            DisplayConnection::Wayland(_) => {}
            DisplayConnection::Unavailable { reason } => {
                assert!(!reason.is_empty(), "an unavailable display must say why");
            }
        }
    }
}
