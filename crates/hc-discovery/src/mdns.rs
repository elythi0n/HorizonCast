//! mDNS / Bonjour discovery of AirPlay devices (`_airplay._tcp`).
//!
//! `mdns-sd` runs its own background threads with a synchronous receiver, so the browse
//! loop runs on a blocking task and feeds results into the shared registry.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hc_core::Protocol;
use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::registry::{DeviceRegistry, Observation};

const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

/// Strip the service suffix from an mDNS fullname to get the friendly instance label,
/// e.g. `"Living Room._airplay._tcp.local."` -> `"Living Room"`.
#[must_use]
pub fn instance_label(fullname: &str, service: &str) -> String {
    fullname
        .strip_suffix(service)
        .map(|s| s.trim_end_matches('.'))
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_string()
}

/// Browse for AirPlay devices until `window` elapses, feeding the registry. Resilient:
/// if the mDNS daemon can't start (no network / CI sandbox), it logs and returns.
pub async fn browse(registry: Arc<Mutex<DeviceRegistry>>, start: Instant, window: Duration) {
    if let Err(e) =
        tokio::task::spawn_blocking(move || browse_blocking(&registry, start, window)).await
    {
        tracing::warn!(error = %e, "mDNS: browse task join failed");
    }
}

fn browse_blocking(registry: &Arc<Mutex<DeviceRegistry>>, start: Instant, window: Duration) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "mDNS: could not start daemon; skipping");
            return;
        }
    };
    let receiver = match daemon.browse(AIRPLAY_SERVICE) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "mDNS: browse failed");
            let _ = daemon.shutdown();
            return;
        }
    };

    loop {
        let elapsed = start.elapsed();
        if elapsed >= window {
            break;
        }
        let remaining = window - elapsed;
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let name = instance_label(info.get_fullname(), AIRPLAY_SERVICE);
                let stable_id = info.get_fullname().to_string();
                let port = info.get_port();
                let now = start.elapsed();
                let mut reg = registry.lock().expect("registry mutex poisoned");
                for addr in info.get_addresses() {
                    reg.observe(
                        Observation {
                            address: addr.to_ip_addr(),
                            port,
                            name: Some(name.clone()),
                            stable_id: Some(stable_id.clone()),
                            // Presence of `_airplay._tcp` => AirPlay-capable; the exact
                            // mirroring feature bits are confirmed when establishing a session.
                            protocol: Protocol::AirPlayMirror,
                            dlna_control_url: None,
                            dlna_location: None,
                        },
                        now,
                    );
                }
            }
            Ok(_) => {}      // other service events are not interesting here
            Err(_) => break, // timeout (window elapsed) or channel closed
        }
    }
    let _ = daemon.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_label_strips_service_suffix() {
        assert_eq!(
            instance_label("Living Room._airplay._tcp.local.", "_airplay._tcp.local."),
            "Living Room"
        );
    }

    #[test]
    fn instance_label_handles_name_with_dots() {
        assert_eq!(
            instance_label(
                "Sam's TV (Den)._airplay._tcp.local.",
                "_airplay._tcp.local."
            ),
            "Sam's TV (Den)"
        );
    }

    #[test]
    fn instance_label_passthrough_when_suffix_absent() {
        assert_eq!(
            instance_label("WeirdName", "_airplay._tcp.local."),
            "WeirdName"
        );
    }
}
