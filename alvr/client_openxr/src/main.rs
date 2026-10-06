//! The Steam Frame client binary.
//!
//! Android loads this client as a `cdylib` from Java, so the only entry point upstream has is the
//! JNI `android_main` and there is no `main` at all. The Frame is not Android: it runs SteamVR as
//! an OpenXR runtime on gamescope/kwin, and a client there is an ordinary process. This is it.
//!
//! It is deliberately thin. Everything it does after `entry_point` is the same code the Android
//! client runs; the two things a desktop session needs that a Java host supplied for free — a
//! display connection, and a loader that is not on the default search path — are the whole
//! difference, and both are reported here in words rather than as a crash.

#[cfg(target_os = "android")]
fn main() {
    // Not part of the Android build: that client is the `cdylib` in `c_api.rs`, loaded by the
    // Java host. This exists so `cargo build` is honest about both targets rather than failing
    // to find a `main` it was never meant to run.
}

#[cfg(not(target_os = "android"))]
fn main() {
    // Exit codes, so a launcher or a `systemd` unit can tell *why* the client stopped without
    // parsing the log. Chosen so that "the environment is wrong" is not the same failure as
    // "the runtime rejected us".
    const EXIT_NO_DISPLAY: i32 = 2;

    // Logging first. A startup failure that prints nothing is indistinguishable from a hang, and
    // this is the entry point most likely to fail on a device nobody has run the client on.
    alvr_client_core::init_logging();

    alvr_common::info!(
        "GemLink client {} — {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    // The display is the one precondition this process can check, and report, before it hands
    // control to the runtime. Everything else — whether the loader exists, whether the runtime
    // will talk to us — is better answered by the OpenXR loader in its own words.
    let display = alvr_client_openxr::DisplayConnection::discover();
    if let Some(reason) = display.unavailable_reason() {
        alvr_common::error!("Cannot start: {reason}");
        std::process::exit(EXIT_NO_DISPLAY);
    }

    alvr_common::smoke("display", || {
        "display connection ready — handing off to the OpenXR loader".to_string()
    });

    alvr_client_openxr::entry_point(display);
}
