//! `hc-net` — shared networking primitives.
//!
//! Houses the UPnP/DLNA control client ([`upnp`]) and will grow the local HTTP media
//! server, RTSP/RTP and MPEG-TS packetizers, and the AirPlay crypto primitives. Shared
//! by the cast sinks and by discovery (for device-description enrichment).

pub mod addr;
pub mod bplist;
pub mod live_stream;
pub mod media_server;
pub mod mpegts;
pub mod rtsp;
pub mod upnp;
