//! The long-running loop around [`AutoSwitchEngine::tick`], and the
//! [`LoopHandle`] other threads (signal watcher, TUI, menubar) use to stop
//! it, cut its sleep short or retarget its threshold.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{AutoSwitchEngine, EventKind};
use crate::store::format_timestamp;

#[derive(Default)]
struct Flags {
    stopped: bool,
    woken: bool,
    threshold: Option<f64>,
}

/// Cloneable, `Send + Sync` control for one engine's loop. Build it first
/// and hand a clone to [`AutoSwitchEngine::loop_handle`] when the engine
/// itself has to live on another thread.
#[derive(Clone, Default)]
pub struct LoopHandle(Arc<(Mutex<Flags>, Condvar)>);

impl LoopHandle {
    pub fn new() -> Self {
        Self::default()
    }

    fn flags(&self) -> MutexGuard<'_, Flags> {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends the loop after the tick in flight. Permanent: a stop issued
    /// before the loop starts makes it return without ticking.
    pub fn stop(&self) {
        let mut flags = self.flags();
        flags.stopped = true;
        flags.woken = true;
        self.0.1.notify_all();
    }

    /// Cuts the current (or next) sleep short.
    pub fn wake(&self) {
        self.flags().woken = true;
        self.0.1.notify_all();
    }

    /// Session-only threshold for the loop's following ticks; pair with
    /// [`wake`](Self::wake) to apply it now.
    pub fn apply_threshold(&self, threshold: f64) {
        self.flags().threshold = Some(threshold);
    }

    pub fn is_stopped(&self) -> bool {
        self.flags().stopped
    }

    /// Clears the wake at the top of a pass, so a wake that lands during the
    /// tick still cuts the following sleep short.
    fn begin_pass(&self) -> (bool, Option<f64>) {
        let mut flags = self.flags();
        flags.woken = false;
        (flags.stopped, flags.threshold.take())
    }

    fn wait(&self, timeout: Duration) {
        let deadline = Instant::now().checked_add(timeout);
        let mut flags = self.flags();
        while !flags.woken {
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if remaining.is_zero() {
                return;
            }
            flags = match self.0.1.wait_timeout(flags, remaining) {
                Ok((guard, _)) => guard,
                Err(e) => e.into_inner().0,
            };
        }
    }
}

impl AutoSwitchEngine<'_> {
    /// Shares `handle` as this engine's loop control.
    pub fn loop_handle(mut self, handle: LoopHandle) -> Self {
        self.handle = handle;
        self
    }

    pub fn handle(&self) -> LoopHandle {
        self.handle.clone()
    }

    pub fn stop(&self) {
        self.handle.stop();
    }

    pub fn wake(&self) {
        self.handle.wake();
    }

    /// Retargets the switch trigger and the poll planning; the models stay
    /// as constructed.
    pub fn apply_threshold(&mut self, threshold: f64) {
        self.settings.threshold = threshold;
        self.host.set_poll_policy_inputs(threshold, &self.models);
    }

    /// Ticks until stopped; returns the exit code (always 0). A failing tick
    /// is an `error` event, never the end of the loop.
    pub fn run_loop(&mut self) -> i32 {
        loop {
            let (stopped, threshold) = self.handle.begin_pass();
            if stopped {
                return 0;
            }
            if let Some(t) = threshold {
                self.apply_threshold(t);
            }
            let outcome = self.tick();
            let delay = self.next_delay(outcome);
            if delay > self.settings.interval_seconds * 1.5 {
                let until = format_timestamp((self.now() + delay).floor() as i64);
                self.emit(EventKind::Sleep {
                    seconds: delay,
                    until,
                });
            }
            self.handle
                .wait(Duration::try_from_secs_f64(delay).unwrap_or(Duration::ZERO));
        }
    }
}
