//! In-memory registry that merges device observations from multiple discovery
//! backends (mDNS, SSDP) into a unified, de-duplicated, preference-ordered list.
//!
//! Observations are tracked per network address, but a single physical device often
//! appears under several addresses (IPv4 + IPv6 link-local) and across protocols
//! (AirPlay via mDNS, DLNA via SSDP). The device list therefore *coalesces* addresses
//! that share any stable identity (an mDNS instance name or an SSDP USN) into one
//! device, preferring the routable IPv4 address as primary.
//!
//! All time is passed in explicitly as a monotonic `Duration` (elapsed since the scan
//! began) so the merge/expiry logic is fully deterministic and unit-testable.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::time::Duration;

use hc_core::{Device, Protocol};

/// A single sighting of a device from one discovery backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Network address the device was seen at.
    pub address: IpAddr,
    /// Control/description port (0 if unknown from this sighting).
    pub port: u16,
    /// Friendly name, if the backend provided one.
    pub name: Option<String>,
    /// Stable identifier from the backend (mDNS instance / SSDP USN), if any. Used to
    /// recognise the same physical device across addresses and protocols.
    pub stable_id: Option<String>,
    /// The transport this sighting implies.
    pub protocol: Protocol,
    /// Absolute AVTransport control URL, if this sighting resolved one (DLNA).
    pub dlna_control_url: Option<String>,
    /// DLNA device-description URL (SSDP `LOCATION`), if known.
    pub dlna_location: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    name: Option<String>,
    /// All stable identities seen at this address (mDNS names + SSDP USNs).
    identities: BTreeSet<String>,
    port: u16,
    protocols: Vec<Protocol>,
    dlna_control_url: Option<String>,
    dlna_location: Option<String>,
    last_seen: Duration,
}

/// Merges observations keyed by IP address and expires stale entries.
#[derive(Debug)]
pub struct DeviceRegistry {
    ttl: Duration,
    entries: HashMap<IpAddr, Entry>,
}

impl DeviceRegistry {
    /// Create a registry whose entries expire after `ttl` without a fresh sighting.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    /// Record an observation, merging it into any existing entry for the same address.
    /// `now` is monotonic elapsed time supplied by the caller.
    pub fn observe(&mut self, obs: Observation, now: Duration) {
        let entry = self.entries.entry(obs.address).or_insert_with(|| Entry {
            name: None,
            identities: BTreeSet::new(),
            port: 0,
            protocols: Vec::new(),
            dlna_control_url: None,
            dlna_location: None,
            last_seen: now,
        });

        entry.last_seen = now;

        if entry.port == 0 && obs.port != 0 {
            entry.port = obs.port;
        }

        // Keep the first non-empty name; never clobber a good name with an empty one.
        if let Some(name) = obs.name
            && !name.is_empty()
            && entry.name.as_deref().is_none_or(str::is_empty)
        {
            entry.name = Some(name);
        }

        if entry.dlna_control_url.is_none()
            && let Some(url) = obs.dlna_control_url
            && !url.is_empty()
        {
            entry.dlna_control_url = Some(url);
        }

        if entry.dlna_location.is_none()
            && let Some(loc) = obs.dlna_location
            && !loc.is_empty()
        {
            entry.dlna_location = Some(loc);
        }

        if let Some(id) = obs.stable_id
            && !id.is_empty()
        {
            entry.identities.insert(id);
        }

        if !entry.protocols.contains(&obs.protocol) {
            entry.protocols.push(obs.protocol);
        }
    }

    /// Remove entries not seen within the TTL window relative to `now`.
    pub fn expire(&mut self, now: Duration) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, e| now.saturating_sub(e.last_seen) <= ttl);
    }

    /// Current devices: addresses sharing an identity are coalesced into one device
    /// (protocols unioned, routable IPv4 chosen as primary), the list ordered by best
    /// protocol then name then address — stable and deterministic.
    #[must_use]
    pub fn devices(&self) -> Vec<Device> {
        // Deterministic address order.
        let mut addrs: Vec<IpAddr> = self.entries.keys().copied().collect();
        addrs.sort();

        // Union-find: link addresses that share any stable identity.
        let mut parent: Vec<usize> = (0..addrs.len()).collect();
        let mut id_owner: HashMap<&str, usize> = HashMap::new();
        for (i, addr) in addrs.iter().enumerate() {
            for id in &self.entries[addr].identities {
                match id_owner.get(id.as_str()) {
                    Some(&j) => {
                        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                        if a != b {
                            parent[a] = b;
                        }
                    }
                    None => {
                        id_owner.insert(id.as_str(), i);
                    }
                }
            }
        }

        // Group addresses by component root.
        let mut groups: HashMap<usize, Vec<IpAddr>> = HashMap::new();
        for (i, addr) in addrs.iter().enumerate() {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(*addr);
        }

        let mut devices: Vec<Device> = groups.values().map(|g| self.merge_group(g)).collect();
        devices.sort_by(|a, b| {
            let ra = a.protocols.first().map_or(u8::MAX, |p| preference_rank(*p));
            let rb = b.protocols.first().map_or(u8::MAX, |p| preference_rank(*p));
            ra.cmp(&rb)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.address.cmp(&b.address))
        });
        devices
    }

    /// Merge a group of addresses (one physical device) into a single [`Device`].
    fn merge_group(&self, group: &[IpAddr]) -> Device {
        let primary = *group
            .iter()
            .min_by(|a, b| address_rank(**a).cmp(&address_rank(**b)).then(a.cmp(b)))
            .expect("group is non-empty");
        let port = self.entries[&primary].port;

        let mut sorted = group.to_vec();
        sorted.sort();

        let mut protocols: Vec<Protocol> = Vec::new();
        for addr in &sorted {
            for p in &self.entries[addr].protocols {
                if !protocols.contains(p) {
                    protocols.push(*p);
                }
            }
        }
        protocols.sort_by_key(|p| preference_rank(*p));

        let name = sorted
            .iter()
            .find_map(|a| self.entries[a].name.clone().filter(|n| !n.is_empty()))
            .unwrap_or_else(|| format!("Unknown device ({primary})"));

        let dlna_control_url = sorted
            .iter()
            .find_map(|a| self.entries[a].dlna_control_url.clone());

        let dlna_location = sorted
            .iter()
            .find_map(|a| self.entries[a].dlna_location.clone());

        let id = sorted
            .iter()
            .flat_map(|a| self.entries[a].identities.iter())
            .min()
            .cloned()
            .unwrap_or_else(|| primary.to_string());

        Device {
            id,
            name,
            address: primary,
            port,
            protocols,
            dlna_control_url,
            dlna_location,
        }
    }

    /// Number of distinct network endpoints (addresses) currently tracked. Note this
    /// counts addresses, not coalesced devices (use [`devices`](Self::devices) for that).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Iterative union-find root lookup with path halving.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Lower rank = more preferred as the default transport (low-latency mirror first).
#[must_use]
pub fn preference_rank(p: Protocol) -> u8 {
    match p {
        Protocol::AirPlayMirror => 0,
        Protocol::AirPlayVideo => 1,
        Protocol::Cast => 2,
        Protocol::Miracast => 3,
        Protocol::Dlna => 4,
    }
}

/// Lower rank = preferred as a device's primary address. Routable IPv4 beats global
/// IPv6, which beats IPv6 link-local; loopback is last.
fn address_rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => 3,
        IpAddr::V4(_) => 0,
        IpAddr::V6(v6) if v6.is_loopback() => 3,
        // fe80::/10 link-local.
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => 2,
        IpAddr::V6(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, d))
    }

    fn obs(addr: IpAddr, name: Option<&str>, proto: Protocol, port: u16) -> Observation {
        Observation {
            address: addr,
            port,
            name: name.map(str::to_string),
            stable_id: None,
            protocol: proto,
            dlna_control_url: None,
            dlna_location: None,
        }
    }

    fn obs_id(
        addr: IpAddr,
        name: Option<&str>,
        proto: Protocol,
        port: u16,
        id: &str,
    ) -> Observation {
        Observation {
            stable_id: Some(id.to_string()),
            ..obs(addr, name, proto, port)
        }
    }

    #[test]
    fn single_observation_yields_one_device() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(
            obs(ip(10), Some("Samsung TV"), Protocol::AirPlayMirror, 7000),
            Duration::ZERO,
        );
        let devices = reg.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Samsung TV");
        assert_eq!(devices[0].port, 7000);
        assert_eq!(devices[0].protocols, vec![Protocol::AirPlayMirror]);
    }

    #[test]
    fn same_ip_merges_protocols_ordered_by_preference() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(10), None, Protocol::Dlna, 9197), Duration::ZERO);
        reg.observe(
            obs(
                ip(10),
                Some("Living Room TV"),
                Protocol::AirPlayMirror,
                7000,
            ),
            Duration::from_secs(1),
        );
        let devices = reg.devices();
        assert_eq!(devices.len(), 1, "same IP must merge into one device");
        assert_eq!(devices[0].name, "Living Room TV");
        assert_eq!(
            devices[0].protocols,
            vec![Protocol::AirPlayMirror, Protocol::Dlna]
        );
    }

    #[test]
    fn duplicate_protocol_is_not_added_twice() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(10), None, Protocol::Dlna, 0), Duration::ZERO);
        reg.observe(obs(ip(10), None, Protocol::Dlna, 0), Duration::from_secs(1));
        assert_eq!(reg.devices()[0].protocols, vec![Protocol::Dlna]);
    }

    #[test]
    fn good_name_is_not_overwritten_by_empty() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(
            obs(ip(10), Some("Real Name"), Protocol::AirPlayMirror, 7000),
            Duration::ZERO,
        );
        reg.observe(
            obs(ip(10), Some(""), Protocol::Dlna, 0),
            Duration::from_secs(1),
        );
        assert_eq!(reg.devices()[0].name, "Real Name");
    }

    #[test]
    fn port_is_backfilled_when_first_sighting_had_none() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(10), None, Protocol::Dlna, 0), Duration::ZERO);
        reg.observe(
            obs(ip(10), None, Protocol::AirPlayMirror, 7000),
            Duration::from_secs(1),
        );
        assert_eq!(reg.devices()[0].port, 7000);
    }

    #[test]
    fn unknown_name_falls_back_to_address() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(42), None, Protocol::Dlna, 9197), Duration::ZERO);
        assert_eq!(reg.devices()[0].name, "Unknown device (192.168.1.42)");
    }

    #[test]
    fn expiry_removes_stale_but_keeps_fresh() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(1), Some("Old"), Protocol::Dlna, 0), Duration::ZERO);
        reg.observe(
            obs(ip(2), Some("New"), Protocol::Dlna, 0),
            Duration::from_secs(8),
        );
        reg.expire(Duration::from_secs(12));
        let names: Vec<_> = reg.devices().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["New".to_string()]);
    }

    #[test]
    fn refreshing_keeps_device_alive() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(1), Some("TV"), Protocol::Dlna, 0), Duration::ZERO);
        reg.observe(
            obs(ip(1), Some("TV"), Protocol::Dlna, 0),
            Duration::from_secs(8),
        );
        reg.expire(Duration::from_secs(12));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn list_ordered_airplay_before_dlna_then_by_name() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(
            obs(ip(1), Some("Zeta DLNA"), Protocol::Dlna, 0),
            Duration::ZERO,
        );
        reg.observe(
            obs(ip(2), Some("Alpha AirPlay"), Protocol::AirPlayMirror, 7000),
            Duration::ZERO,
        );
        reg.observe(
            obs(ip(3), Some("Beta AirPlay"), Protocol::AirPlayMirror, 7000),
            Duration::ZERO,
        );
        let names: Vec<_> = reg.devices().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "Alpha AirPlay".to_string(),
                "Beta AirPlay".to_string(),
                "Zeta DLNA".to_string(),
            ]
        );
    }

    #[test]
    fn control_url_is_captured_and_kept() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(10));
        reg.observe(obs(ip(10), None, Protocol::Dlna, 9197), Duration::ZERO);
        let enriched = Observation {
            name: Some("Samsung TV".into()),
            dlna_control_url: Some("http://192.168.1.10:9197/upnp/control/AVTransport1".into()),
            ..obs(ip(10), None, Protocol::Dlna, 0)
        };
        reg.observe(enriched, Duration::from_secs(1));
        let d = &reg.devices()[0];
        assert_eq!(d.name, "Samsung TV");
        assert_eq!(
            d.dlna_control_url.as_deref(),
            Some("http://192.168.1.10:9197/upnp/control/AVTransport1")
        );
    }

    #[test]
    fn same_device_on_ipv4_and_ipv6_coalesces_with_ipv4_primary() {
        // Mirrors the real-hardware case: one TV seen on IPv4 (AirPlay + DLNA) and on
        // its IPv6 link-local (AirPlay), sharing one mDNS instance name.
        let mut reg = DeviceRegistry::new(Duration::from_secs(30));
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 158));
        let v6: IpAddr = "fe80::96e6:baff:fe1d:e02a".parse().unwrap();
        let mdns = "55\" Crystal UHD._airplay._tcp.local.";

        reg.observe(
            obs_id(
                v4,
                Some("55\" Crystal UHD"),
                Protocol::AirPlayMirror,
                7000,
                mdns,
            ),
            Duration::ZERO,
        );
        reg.observe(
            obs_id(
                v6,
                Some("55\" Crystal UHD"),
                Protocol::AirPlayMirror,
                7000,
                mdns,
            ),
            Duration::ZERO,
        );
        // DLNA on the IPv4 address carries a different identity (the SSDP USN).
        reg.observe(
            obs_id(v4, None, Protocol::Dlna, 9197, "uuid:abcd-1234"),
            Duration::from_secs(1),
        );

        let devices = reg.devices();
        assert_eq!(devices.len(), 1, "one physical TV, not multiple entries");
        assert_eq!(devices[0].address, v4, "primary must be the routable IPv4");
        assert_eq!(
            devices[0].protocols,
            vec![Protocol::AirPlayMirror, Protocol::Dlna]
        );
        assert_eq!(devices[0].name, "55\" Crystal UHD");
    }

    #[test]
    fn coalescing_is_order_independent() {
        // DLNA (USN) seen on IPv4 *before* mDNS still groups with the IPv6 mDNS entry.
        let mut reg = DeviceRegistry::new(Duration::from_secs(30));
        let v4 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let v6: IpAddr = "fe80::1".parse().unwrap();
        let mdns = "TV._airplay._tcp.local.";

        reg.observe(
            obs_id(v4, None, Protocol::Dlna, 9197, "uuid:x"),
            Duration::ZERO,
        );
        reg.observe(
            obs_id(v4, Some("TV"), Protocol::AirPlayMirror, 7000, mdns),
            Duration::ZERO,
        );
        reg.observe(
            obs_id(v6, Some("TV"), Protocol::AirPlayMirror, 7000, mdns),
            Duration::ZERO,
        );
        assert_eq!(reg.devices().len(), 1);
    }

    #[test]
    fn distinct_devices_are_not_coalesced() {
        let mut reg = DeviceRegistry::new(Duration::from_secs(30));
        reg.observe(
            obs_id(
                ip(1),
                Some("TV A"),
                Protocol::AirPlayMirror,
                7000,
                "A._airplay._tcp.local.",
            ),
            Duration::ZERO,
        );
        reg.observe(
            obs_id(
                ip(2),
                Some("TV B"),
                Protocol::AirPlayMirror,
                7000,
                "B._airplay._tcp.local.",
            ),
            Duration::ZERO,
        );
        assert_eq!(reg.devices().len(), 2);
    }

    #[test]
    fn preference_rank_orders_low_latency_first() {
        assert!(preference_rank(Protocol::AirPlayMirror) < preference_rank(Protocol::AirPlayVideo));
        assert!(preference_rank(Protocol::AirPlayVideo) < preference_rank(Protocol::Dlna));
        assert!(preference_rank(Protocol::Cast) < preference_rank(Protocol::Dlna));
    }

    #[test]
    fn empty_registry_reports_empty() {
        let reg = DeviceRegistry::new(Duration::from_secs(10));
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.devices().is_empty());
    }
}
