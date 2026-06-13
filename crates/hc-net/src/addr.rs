//! Local address selection.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use hc_core::{Error, Result};

/// Determine which local IP address the OS would use to reach `target`.
///
/// Opens a UDP socket and `connect`s it to the target (no packets are actually sent);
/// the socket's local address is then the source IP the OS would route from. This is
/// the address a TV must use to reach our media server — more reliable than guessing
/// among multiple network interfaces.
pub async fn local_ip_for(target: IpAddr) -> Result<IpAddr> {
    let bind: IpAddr = if target.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    let sock = tokio::net::UdpSocket::bind((bind, 0))
        .await
        .map_err(|e| Error::Sink(format!("local-ip probe bind failed: {e}")))?;
    // Port 9 (discard); UDP connect just fixes the peer and selects a route.
    sock.connect((target, 9))
        .await
        .map_err(|e| Error::Sink(format!("local-ip probe connect failed: {e}")))?;
    sock.local_addr()
        .map(|a| a.ip())
        .map_err(|e| Error::Sink(format!("local-ip probe addr failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_ip_for_loopback_is_loopback() {
        // Reaching a loopback target must route from loopback — deterministic offline.
        let ip = local_ip_for(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
}
