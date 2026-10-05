//! Termination-signal handling.
//!
//! The app restores playback routing on its way out (`shutdown` in `main.rs`
//! and the window's `close-request`), which is what stops the virtual sink
//! from taking every player's audio down with it. But that cleanup only runs if
//! the process is allowed to shut down, and `kill`, `pkill` and the session
//! manager stopping a user unit all use SIGTERM, whose default action is to die
//! on the spot. The user then closes the app with a keyboard shortcut or a
//! `kill` and the audio stops.
//!
//! So the signal itself is caught, but only as a flag: the update loop already
//! runs every 33 ms, and it is the only place that can quit the application
//! through GTK properly. The handler does nothing but an atomic store, which is
//! all a signal handler is allowed to do anyway; touching PipeWire from one
//! would mean calling into `Rc`-based, main-thread-only code from async
//! signal context.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set when SIGTERM or SIGINT arrives. Read by the update loop, which turns it
/// into a normal `Application::quit()`.
static TERMINATE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Signal handler. Does the only thing a handler may do here: store to an
/// atomic. No allocation, no locking, no logging.
extern "C" fn note_termination(_signum: libc::c_int) {
    TERMINATE_REQUESTED.store(true, Ordering::SeqCst);
}

/// Catch SIGTERM and SIGINT, recording the request instead of dying.
///
/// Without this, GLib's default action terminates the process immediately and
/// the routing restore never runs.
pub fn install_signal_handlers() {
    for signum in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `note_termination` is a plain `extern "C" fn` that only
        // stores to a static atomic, which is async-signal-safe.
        unsafe {
            libc::signal(signum, note_termination as *const () as libc::sighandler_t);
        }
    }
}

/// True once a termination signal has been caught. Cleared by
/// [`take_terminate_request`] so the shutdown happens exactly once.
pub fn terminate_requested() -> bool {
    TERMINATE_REQUESTED.load(Ordering::SeqCst)
}

/// Consume the pending termination request.
pub fn take_terminate_request() -> bool {
    TERMINATE_REQUESTED.swap(false, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_starts_clear_and_round_trips() {
        // Take whatever a previous test left, then assert the contract: read
        // only ever reports what was stored, and taking it clears it.
        let _ = take_terminate_request();
        assert!(!terminate_requested());
        TERMINATE_REQUESTED.store(true, Ordering::SeqCst);
        assert!(terminate_requested());
        assert!(take_terminate_request());
        assert!(!terminate_requested());
        assert!(!take_terminate_request());
    }
}
