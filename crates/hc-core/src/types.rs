//! Shared value types used across the capture → encode → sink pipeline.

use std::net::IpAddr;
use std::time::Duration;

/// A casting transport that a device can speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// AirPlay 2 screen mirroring (low-latency mirror).
    AirPlayMirror,
    /// AirPlay video — push a media URL for native playback.
    AirPlayVideo,
    /// DLNA / UPnP AV media renderer.
    Dlna,
    /// Miracast (Wi-Fi Direct mirror) — Windows sender only.
    Miracast,
    /// Google Cast.
    Cast,
}

/// A discovered cast target on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable identifier (e.g. mDNS/USN name).
    pub id: String,
    /// Human-readable name to show in the UI.
    pub name: String,
    /// Network address.
    pub address: IpAddr,
    /// Control port.
    pub port: u16,
    /// Transports this device advertises, best-first.
    pub protocols: Vec<Protocol>,
    /// Absolute AVTransport control URL (DLNA devices only), resolved from the device
    /// description. `None` for devices without a known DLNA control endpoint.
    pub dlna_control_url: Option<String>,
    /// URL of the DLNA device-description document (from SSDP `LOCATION`). Retained so
    /// the control URL can be recovered on demand if description enrichment failed.
    pub dlna_location: Option<String>,
}

/// Monotonic capture-clock timestamp shared by audio and video, for A/V sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub Duration);

/// Video codec for an encoded unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC (required by AirPlay mirroring).
    H264,
    /// H.265 / HEVC (higher quality where the TV supports it).
    Hevc,
}

/// Audio codec for an encoded unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC.
    Aac,
    /// Linear PCM.
    Pcm,
}

/// Requested video capture/encode configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Target frames per second.
    pub fps: u32,
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Codec to encode with.
    pub codec: VideoCodec,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            codec: VideoCodec::H264,
        }
    }
}

/// A captured video frame backed by a platform GPU surface.
///
/// For now this carries only metadata; backends will attach a zero-copy GPU surface
/// handle (`IOSurface` / `D3D11Texture2D` / `DMA-BUF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrame {
    /// Presentation timestamp on the shared capture clock.
    pub pts: Timestamp,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// A captured chunk of system audio (interleaved PCM in the skeleton).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// Presentation timestamp on the shared capture clock.
    pub pts: Timestamp,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u8,
    /// Interleaved signed 16-bit samples.
    pub samples: Vec<i16>,
}

/// One encoded, ready-to-send media unit (e.g. an H.264 access unit or AAC frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedUnit {
    /// Presentation timestamp on the shared capture clock.
    pub pts: Timestamp,
    /// Whether this unit is a keyframe / sync sample.
    pub keyframe: bool,
    /// Encoded bytes (Annex-B for H.264/HEVC video units).
    pub data: Vec<u8>,
}

/// What a given [`crate::CastSink`] can accept, used by core to negotiate quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkCaps {
    /// Video codecs the sink accepts.
    pub video_codecs: Vec<VideoCodec>,
    /// Audio codecs the sink accepts.
    pub audio_codecs: Vec<AudioCodec>,
    /// Maximum width the sink accepts.
    pub max_width: u32,
    /// Maximum height the sink accepts.
    pub max_height: u32,
    /// Maximum frame rate the sink accepts.
    pub max_fps: u32,
    /// True if the sink wants a muxed container/URL (DLNA) rather than raw units
    /// (AirPlay mirror). Drives which pipeline core runs.
    pub needs_container: bool,
}
