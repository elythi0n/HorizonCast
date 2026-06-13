//! `hc-discovery` — local-network device discovery.
//!
//! Browses mDNS (`_airplay._tcp`) and SSDP (UPnP `MediaRenderer`) concurrently and
//! merges the results into a unified, de-duplicated, preference-ordered
//! [`hc_core::Device`] list (AirPlay preferred when a TV advertises both).
//!
//! The merge/expiry logic lives in [`DeviceRegistry`] and is fully deterministic and
//! unit-tested; the per-backend socket loops in [`mdns`] and [`ssdp`] are thin and
//! resilient — if one backend can't run (no network, blocked multicast) the other
//! still contributes, and discovery never panics.

pub mod mdns;
pub mod registry;
pub mod service;
pub mod ssdp;

pub use registry::{DeviceRegistry, Observation, preference_rank};
pub use service::Discovery;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hc_core::Device;

/// How long a device may go unseen before it is considered gone.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15);

/// Discover cast devices on the local network for `window`, returning a merged,
/// de-duplicated, preference-ordered list.
///
/// Both discovery backends run concurrently and stop when `window` elapses.
pub async fn discover_for(window: Duration) -> Vec<Device> {
    let registry = Arc::new(Mutex::new(DeviceRegistry::new(DEFAULT_TTL)));
    let start = Instant::now();

    let mdns_task = tokio::spawn(mdns::browse(registry.clone(), start, window));
    let ssdp_task = tokio::spawn(ssdp::search(registry.clone(), start, window));

    // Both tasks self-terminate at `window`; failures are already logged internally.
    let _ = mdns_task.await;
    let _ = ssdp_task.await;

    let devices = registry.lock().expect("registry mutex poisoned").devices();
    tracing::info!(count = devices.len(), "discovery window complete");
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery must complete promptly and not panic even with no devices / no usable
    /// network (CI sandbox). This exercises the full concurrent plumbing end-to-end.
    #[tokio::test]
    async fn discover_for_is_bounded_and_resilient() {
        let start = Instant::now();
        let _devices = discover_for(Duration::from_millis(300)).await;
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "discovery should respect its window and not hang"
        );
    }
}
