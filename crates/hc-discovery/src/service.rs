//! Continuous discovery service.
//!
//! Runs mDNS + SSDP on a loop against one persistent [`DeviceRegistry`], expiring stale
//! entries each pass and publishing the current device set to subscribers whenever it
//! changes. This is what a live-updating UI consumes; the one-shot [`crate::discover_for`]
//! remains for quick scans.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hc_core::Device;
use tokio::sync::watch;

use crate::registry::DeviceRegistry;
use crate::{DEFAULT_TTL, mdns, ssdp};

/// How long each scan pass runs before the list is expired and (if changed) republished.
const SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// A running discovery service. While alive it continuously scans; read the current list
/// with [`Discovery::current`] or react to changes via [`Discovery::subscribe`].
pub struct Discovery {
    registry: Arc<Mutex<DeviceRegistry>>,
    devices_rx: watch::Receiver<Vec<Device>>,
    shutdown: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Discovery {
    /// Start discovery with the default scan interval.
    #[must_use]
    pub fn start() -> Self {
        Self::start_with_interval(SCAN_INTERVAL)
    }

    /// Start discovery with a custom scan interval.
    #[must_use]
    pub fn start_with_interval(interval: Duration) -> Self {
        let registry = Arc::new(Mutex::new(DeviceRegistry::new(DEFAULT_TTL)));
        let (tx, rx) = watch::channel(Vec::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run(registry.clone(), tx, shutdown.clone(), interval));
        Self {
            registry,
            devices_rx: rx,
            shutdown,
            handle,
        }
    }

    /// The current device-list snapshot.
    #[must_use]
    pub fn current(&self) -> Vec<Device> {
        self.registry
            .lock()
            .expect("registry mutex poisoned")
            .devices()
    }

    /// Subscribe to device-list changes. `changed()` on the receiver resolves whenever
    /// the set changes; `borrow()` reads the latest snapshot.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Vec<Device>> {
        self.devices_rx.clone()
    }

    /// Stop scanning and wait for the background task to finish (bounded by one pass).
    pub async fn stop(self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.handle.await;
    }
}

async fn run(
    registry: Arc<Mutex<DeviceRegistry>>,
    tx: watch::Sender<Vec<Device>>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
) {
    // One monotonic clock for the whole service lifetime, so TTL/expiry is consistent.
    let start = Instant::now();
    let mut last: Vec<Device> = Vec::new();

    while !shutdown.load(Ordering::Acquire) {
        let deadline = start.elapsed() + interval;
        tokio::join!(
            mdns::browse(registry.clone(), start, deadline),
            ssdp::search(registry.clone(), start, deadline),
        );

        let snapshot = {
            let mut reg = registry.lock().expect("registry mutex poisoned");
            reg.expire(start.elapsed());
            reg.devices()
        };
        if snapshot != last {
            last.clone_from(&snapshot);
            // Ignore send errors: a closed channel just means no subscribers.
            let _ = tx.send(snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_is_bounded_and_resilient() {
        // Runs the full scan loop with a short interval; must not panic with no network
        // (CI/WSL), expose a snapshot, and stop promptly.
        let disco = Discovery::start_with_interval(Duration::from_millis(150));
        let _rx = disco.subscribe();
        let _snapshot = disco.current();
        tokio::time::sleep(Duration::from_millis(400)).await;

        let stopping = Instant::now();
        disco.stop().await;
        assert!(
            stopping.elapsed() < Duration::from_secs(3),
            "stop should return within roughly one scan pass"
        );
    }

    #[tokio::test]
    async fn multiple_subscribers_share_state() {
        let disco = Discovery::start_with_interval(Duration::from_millis(150));
        let rx1 = disco.subscribe();
        let rx2 = disco.subscribe();
        // Both start from the same published snapshot.
        assert_eq!(*rx1.borrow(), *rx2.borrow());
        disco.stop().await;
    }
}
