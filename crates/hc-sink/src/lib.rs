//! `hc-sink` — cast transports.
//!
//! Implements the cast flows for each transport: `airplay` (AirPlay 2 mirroring
//! sender), `dlna` (UPnP media renderer + local HTTP stream), and later `miracast`
//! and `cast`.

pub mod dlna;
