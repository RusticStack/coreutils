// Skua spike (sh16 P1): support for running a utility's `uumain` inside a host process.
//
// One tool at a time: the host holds a global lock around `begin`..`end`, so plain statics are
// enough. `exit` replaces `std::process::exit` on the paths a tool can reach (broken pipe,
// Skua's 141): inside a host it unwinds to the host's `catch_unwind` instead of killing it.
// With `panic = "abort"` there is nothing to unwind to and `exit` aborts the host.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Unwind payload for an `exit` taken inside a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExit(pub i32);

static ACTIVE: AtomicBool = AtomicBool::new(false);
static NAME: Mutex<Option<&'static str>> = Mutex::new(None);

/// Marks the start of an in-process run of the utility `name`: its messages say `name:`, its
/// exit code starts at 0.
pub fn begin(name: &'static str) {
    if let Ok(mut n) = NAME.lock() {
        *n = Some(name);
    }
    crate::error::set_exit_code(0);
    ACTIVE.store(true, Ordering::SeqCst);
}

/// Marks the end of an in-process run.
pub fn end() {
    ACTIVE.store(false, Ordering::SeqCst);
    if let Ok(mut n) = NAME.lock() {
        *n = None;
    }
}

/// Whether a host is running a utility in process.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The utility name set by [`begin`], if a host is running one.
pub fn name_override() -> Option<&'static str> {
    if !active() {
        return None;
    }
    NAME.lock().ok().and_then(|n| *n)
}

/// `std::process::exit`, or, inside a host, an unwind carrying [`ToolExit`].
pub fn exit(code: i32) -> ! {
    if active() {
        std::panic::resume_unwind(Box::new(ToolExit(code)));
    }
    std::process::exit(code)
}
