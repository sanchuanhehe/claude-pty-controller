//! systemd watchdog (ARCHITECTURE §12.5, review OPS-3, issue #8).
//!
//! Under `Type=notify` + `WatchdogSec=`, the controller sends `READY=1` once it's
//! up, then must periodically send `WATCHDOG=1` or systemd restarts it.
//!
//! OPS-3: a naive tokio-timer feeder can't catch a wedged PTY-reader thread —
//! §2 deliberately runs the blocking PTY I/O OFF the runtime, so a hung reader
//! leaves the runtime healthy and the timer keeps petting. So the pet is GATED
//! on per-component heartbeats: a component bumps its `Heartbeat` as it makes
//! progress, and the feeder pets only while all gated components are fresh.
//!
//! Caveat: a legitimately-idle PTY reader is *blocked in read() with no output*
//! — indistinguishable from a wedged one by a heartbeat. So the reader gate is
//! OPT-IN (`CPC_WATCHDOG_READER_STALL_SECS`, default off): with it off, the
//! watchdog detects runtime-level stalls only (the common, real failure — a
//! future blocking the executor stops this very task, so no pet → restart).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sd_notify::NotifyState;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A last-progress timestamp a component bumps to prove liveness.
#[derive(Clone)]
pub struct Heartbeat(Arc<AtomicU64>);

impl Default for Heartbeat {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(now_ms())))
    }
}

impl Heartbeat {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn beat(&self) {
        self.0.store(now_ms(), Ordering::Relaxed);
    }
    pub fn age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.0.load(Ordering::Relaxed))
    }
}

/// A gated component: pet is withheld if `hb.age_ms() > max_stale_ms`.
pub struct Gate {
    pub name: String,
    pub hb: Heartbeat,
    pub max_stale_ms: u64,
}

/// Names of gates that are stale (pure, testable).
pub fn stale(gates: &[Gate], age: impl Fn(&Heartbeat) -> u64) -> Vec<&str> {
    gates.iter().filter(|g| age(&g.hb) > g.max_stale_ms).map(|g| g.name.as_str()).collect()
}

/// Tell systemd we're up (`READY=1`). No-op when not under `Type=notify`.
pub fn notify_ready() {
    let _ = sd_notify::notify(false, &[NotifyState::Ready]);
}

/// Spawn the watchdog feeder if running under a systemd watchdog; else `None`.
/// Pets at half the configured deadline while all `gates` are fresh.
pub fn spawn(cancel: CancellationToken, gates: Vec<Gate>) -> Option<JoinHandle<()>> {
    let mut usec = 0u64;
    if !sd_notify::watchdog_enabled(false, &mut usec) || usec == 0 {
        tracing::debug!("systemd watchdog not enabled");
        return None;
    }
    let interval = Duration::from_micros(usec / 2);
    tracing::info!(?interval, gates = gates.len(), "systemd watchdog active");
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {
                    let bad = stale(&gates, |h| h.age_ms());
                    if bad.is_empty() {
                        let _ = sd_notify::notify(false, &[NotifyState::Watchdog]);
                    } else {
                        tracing::error!(stale = ?bad, "watchdog: stale component(s); withholding pet → systemd will restart");
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_age_resets_on_beat() {
        let h = Heartbeat::new();
        std::thread::sleep(Duration::from_millis(15));
        assert!(h.age_ms() >= 10);
        h.beat();
        assert!(h.age_ms() < 10);
    }

    #[test]
    fn stale_gate_detection() {
        let fresh = Heartbeat::new();
        let old = Heartbeat::new();
        let gates = vec![
            Gate { name: "fresh".into(), hb: fresh, max_stale_ms: 1000 },
            Gate { name: "old".into(), hb: old, max_stale_ms: 1000 },
        ];
        // Inject ages via the closure (deterministic, no sleeping).
        let bad = stale(&gates, |h| if std::ptr::eq(h.0.as_ref(), gates[1].hb.0.as_ref()) { 5000 } else { 0 });
        assert_eq!(bad, vec!["old"]);
        // all fresh → none stale
        assert!(stale(&gates, |_| 0).is_empty());
    }
}
