//! Shared test-only log capture for `kakehashi::bridge` warnings.
//!
//! `log::set_logger` is process-global and may only succeed ONCE — a second
//! test module installing its own capturing logger panics (or silently
//! captures nothing, depending on run order). Every bridge test that asserts
//! on emitted warnings must therefore share this single logger via
//! [`captured_warnings_for`]. Captures are serialized by an internal lock, so
//! concurrent capture tests never observe each other's messages. Only the
//! calling thread is captured: unrelated tests can log without taking that lock.

use std::cell::Cell;
use std::sync::{Mutex, Once};

use log::{Level, LevelFilter, Log, Metadata, Record};

static LOGGER: CapturingLogger = CapturingLogger {
    messages: Mutex::new(Vec::new()),
};
static INIT_LOGGER: Once = Once::new();
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
thread_local! {
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
}

struct CapturingLogger {
    messages: Mutex<Vec<String>>,
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        CAPTURING.set(false);
    }
}

impl Log for CapturingLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        CAPTURING.get()
            && metadata.level() <= Level::Warn
            && metadata.target() == "kakehashi::bridge"
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = format!("{}:{}:{}", record.level(), record.target(), record.args());
        self.messages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(message);
    }

    fn flush(&self) {}
}

/// Run `f` and return every `kakehashi::bridge` warning (or error) it logged,
/// formatted `LEVEL:target:message`. Serialized across the process: parallel
/// callers block on an internal lock rather than interleave captures.
/// Only logs on the calling thread are captured. `Runtime::block_on` keeps its
/// root future on that thread; spawned tasks and worker threads are excluded.
pub(crate) fn captured_warnings_for<F: FnOnce()>(f: F) -> Vec<String> {
    INIT_LOGGER.call_once(|| {
        log::set_logger(&LOGGER).expect("the shared test logger installs once per process");
        log::set_max_level(LevelFilter::Warn);
    });
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    LOGGER
        .messages
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    CAPTURING.set(true);
    let guard = CaptureGuard;
    f();
    drop(guard);
    let captured = LOGGER
        .messages
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    LOGGER
        .messages
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    captured
}

#[cfg(test)]
mod tests {
    use super::captured_warnings_for;

    #[test]
    fn capture_excludes_other_threads_warning_for_the_same_method() {
        let warnings = captured_warnings_for(|| {
            std::thread::spawn(|| {
                log::warn!(target: "kakehashi::bridge", "codeLens/resolve: unrelated test");
            })
            .join()
            .expect("other test thread finishes while capture is active");
            log::warn!(target: "kakehashi::bridge", "codeLens/resolve: own warning");
        });

        assert_eq!(
            warnings,
            vec!["WARN:kakehashi::bridge:codeLens/resolve: own warning"]
        );
    }
}
