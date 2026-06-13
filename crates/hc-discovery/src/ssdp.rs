//! SSDP (UPnP) discovery of DLNA MediaRenderer devices via multicast M-SEARCH.
//!
//! The parsing helpers are pure and unit-tested; [`search`] is the thin, resilient
//! socket loop that feeds parsed results into the registry.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hc_core::Protocol;
use tokio::net::UdpSocket;
use tokio::time::{sleep, timeout};

use crate::registry::{DeviceRegistry, Observation};

const SSDP_MULTICAST: &str = "239.255.255.250:1900";
const SEARCH_TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

fn msearch_packet() -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 2\r\n\
         ST: {SEARCH_TARGET}\r\n\
         \r\n"
    )
}

/// Parsed subset of an SSDP search response that we care about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SsdpResponse {
    /// URL of the device description document.
    pub location: Option<String>,
    /// Unique Service Name (carries the device UUID).
    pub usn: Option<String>,
    /// Server header (OS/UPnP stack string).
    pub server: Option<String>,
    /// Search/notification target.
    pub st: Option<String>,
}

/// Parse an SSDP/HTTP response into its relevant headers (header keys are
/// case-insensitive per HTTP).
#[must_use]
pub fn parse_ssdp_response(raw: &str) -> SsdpResponse {
    let mut resp = SsdpResponse::default();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "location" => resp.location = Some(value),
            "usn" => resp.usn = Some(value),
            "server" => resp.server = Some(value),
            "st" | "nt" => resp.st = Some(value),
            _ => {}
        }
    }
    resp
}

/// True if the response looks like a DLNA MediaRenderer.
#[must_use]
pub fn is_media_renderer(resp: &SsdpResponse) -> bool {
    let contains = |s: &Option<String>| {
        s.as_deref()
            .is_some_and(|v| v.to_ascii_lowercase().contains("mediarenderer"))
    };
    contains(&resp.st) || contains(&resp.usn)
}

/// Extract the port from a LOCATION URL, e.g. `http://1.2.3.4:9197/desc.xml` -> `9197`.
#[must_use]
pub fn port_from_location(location: &str) -> Option<u16> {
    let after_scheme = location.split("://").nth(1)?;
    let authority = after_scheme.split('/').next()?;
    // IPv6 authorities look like `[::1]:9197`; only treat the last `:` as the port sep.
    let (_, port) = authority.rsplit_once(':')?;
    port.parse().ok()
}

/// Run an SSDP MediaRenderer search until `window` elapses, feeding the registry.
///
/// Resilient by design: if the socket cannot bind or send (no network / blocked
/// multicast, e.g. in CI), it logs and returns rather than panicking, leaving the
/// other discovery backends unaffected.
pub async fn search(registry: Arc<Mutex<DeviceRegistry>>, start: Instant, window: Duration) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "SSDP: could not bind UDP socket; skipping");
            return;
        }
    };

    let packet = msearch_packet();
    // UDP discovery packets can be lost; send a few, spaced out.
    for _ in 0..3 {
        if let Err(e) = socket.send_to(packet.as_bytes(), SSDP_MULTICAST).await {
            tracing::warn!(error = %e, "SSDP: M-SEARCH send failed");
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    // Collect renderers seen during the window (deduped by address). Each is recorded
    // immediately with its DLNA protocol; its description URL is kept for name enrichment.
    let mut renderers: HashMap<IpAddr, Option<String>> = HashMap::new();
    let mut buf = vec![0u8; 2048];
    loop {
        let elapsed = start.elapsed();
        if elapsed >= window {
            break;
        }
        let remaining = window - elapsed;
        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, src))) => {
                let raw = String::from_utf8_lossy(&buf[..n]);
                let resp = parse_ssdp_response(&raw);
                if is_media_renderer(&resp) {
                    let addr = src.ip();
                    let port = resp
                        .location
                        .as_deref()
                        .and_then(port_from_location)
                        .unwrap_or(0);
                    registry.lock().expect("registry mutex poisoned").observe(
                        Observation {
                            address: addr,
                            port,
                            name: None, // resolved from the description document below
                            stable_id: resp.usn.clone(),
                            protocol: Protocol::Dlna,
                            dlna_control_url: None, // resolved during enrichment below
                            dlna_location: resp.location.clone(), // retained for on-demand recovery
                        },
                        start.elapsed(),
                    );
                    renderers.entry(addr).or_insert(resp.location);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "SSDP: recv error");
                break;
            }
            Err(_) => break, // window elapsed
        }
    }

    enrich_names(&registry, renderers, start).await;
}

/// Fetch each renderer's description document concurrently to learn its friendly name,
/// updating the registry. Best-effort: a failed fetch leaves the device with its
/// address-based fallback name.
async fn enrich_names(
    registry: &Arc<Mutex<DeviceRegistry>>,
    renderers: HashMap<IpAddr, Option<String>>,
    start: Instant,
) {
    let mut handles = Vec::new();
    for (addr, location) in renderers {
        let Some(location) = location else { continue };
        let registry = registry.clone();
        handles.push(tokio::spawn(async move {
            match hc_net::upnp::fetch_description(&location).await {
                Ok(xml) => {
                    let desc = hc_net::upnp::parse_device_description(&xml);
                    let control_url = desc
                        .av_transport_control_url
                        .as_deref()
                        .and_then(|c| hc_net::upnp::resolve_url(&location, c));
                    if desc.friendly_name.is_some() || control_url.is_some() {
                        registry.lock().expect("registry mutex poisoned").observe(
                            Observation {
                                address: addr,
                                port: 0,
                                name: desc.friendly_name,
                                stable_id: None,
                                protocol: Protocol::Dlna,
                                dlna_control_url: control_url,
                                dlna_location: None,
                            },
                            start.elapsed(),
                        );
                    }
                }
                Err(e) => tracing::debug!(%addr, error = %e, "SSDP: description fetch failed"),
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMSUNG_RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
        CACHE-CONTROL: max-age=1800\r\n\
        LOCATION: http://192.168.1.50:9197/dmr/SamsungMRDesc.xml\r\n\
        SERVER: SHP, UPnP/1.0, Samsung UPnP SDK/1.0\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        USN: uuid:abcd-1234::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        \r\n";

    #[test]
    fn parses_core_headers_case_insensitively() {
        let resp = parse_ssdp_response(SAMSUNG_RESPONSE);
        assert_eq!(
            resp.location.as_deref(),
            Some("http://192.168.1.50:9197/dmr/SamsungMRDesc.xml")
        );
        assert!(resp.usn.as_deref().unwrap().contains("uuid:abcd-1234"));
        assert!(resp.server.as_deref().unwrap().contains("Samsung"));
        assert!(resp.st.as_deref().unwrap().contains("MediaRenderer"));
    }

    #[test]
    fn lowercase_header_keys_still_parse() {
        let raw = "location: http://10.0.0.2:80/d.xml\r\nst: upnp:rootdevice\r\n";
        let resp = parse_ssdp_response(raw);
        assert_eq!(resp.location.as_deref(), Some("http://10.0.0.2:80/d.xml"));
        assert_eq!(resp.st.as_deref(), Some("upnp:rootdevice"));
    }

    #[test]
    fn identifies_media_renderer() {
        assert!(is_media_renderer(&parse_ssdp_response(SAMSUNG_RESPONSE)));
    }

    #[test]
    fn rejects_non_media_renderer() {
        let raw = "ST: upnp:rootdevice\r\nUSN: uuid:x::upnp:rootdevice\r\n";
        assert!(!is_media_renderer(&parse_ssdp_response(raw)));
    }

    #[test]
    fn extracts_port_from_location() {
        assert_eq!(
            port_from_location("http://192.168.1.50:9197/dmr/desc.xml"),
            Some(9197)
        );
        assert_eq!(port_from_location("http://10.0.0.2:80/d.xml"), Some(80));
    }

    #[test]
    fn port_absent_or_malformed_is_none() {
        assert_eq!(port_from_location("http://192.168.1.50/desc.xml"), None);
        assert_eq!(port_from_location("not a url"), None);
    }

    #[test]
    fn ipv6_authority_takes_last_colon_as_port() {
        assert_eq!(
            port_from_location("http://[fe80::1]:9197/d.xml"),
            Some(9197)
        );
    }

    #[test]
    fn malformed_lines_are_ignored() {
        let resp = parse_ssdp_response("garbage line without colon\r\nLOCATION: http://x/y\r\n");
        assert_eq!(resp.location.as_deref(), Some("http://x/y"));
    }
}
