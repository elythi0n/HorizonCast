//! Core pipeline contracts.
//!
//! Backends in `hc-capture`, `hc-encode`, and `hc-sink` implement these traits; the
//! binaries wire concrete implementations into the core orchestration. All traits are
//! object-safe so the orchestrator can hold `Box<dyn Trait>` and swap backends per OS
//! and per protocol.

use crate::error::Result;
use crate::types::{AudioFrame, Device, EncodedUnit, SinkCaps, VideoConfig, VideoFrame};

/// A source of video frames (a screen-capture backend).
pub trait FrameSource: Send {
    /// Begin capturing with the given configuration.
    fn start(&mut self, cfg: VideoConfig) -> Result<()>;
    /// Non-blocking: return the most recent frame if one is ready, else `None`.
    fn poll(&mut self) -> Option<VideoFrame>;
    /// Stop capturing and release resources.
    fn stop(&mut self);
}

/// A source of system-audio frames.
pub trait AudioSource: Send {
    /// Begin capturing system audio.
    fn start(&mut self) -> Result<()>;
    /// Non-blocking: return the next audio frame if available.
    fn poll(&mut self) -> Option<AudioFrame>;
    /// Stop capturing and release resources.
    fn stop(&mut self);
}

/// A hardware (or software) encoder producing Annex-B H.264/HEVC + audio units.
///
/// Implementations must expose *raw* units (not a muxed container): the AirPlay sink
/// packetizes them itself.
pub trait Encoder: Send {
    /// Encode one video frame into zero or more units.
    fn encode_video(&mut self, frame: VideoFrame) -> Result<Vec<EncodedUnit>>;
    /// Encode one audio frame into zero or more units.
    fn encode_audio(&mut self, frame: AudioFrame) -> Result<Vec<EncodedUnit>>;
    /// Force the next video unit to be a keyframe.
    fn request_keyframe(&mut self);
    /// Adjust bitrate (kbps) and optionally output dimensions for adaptive streaming.
    fn reconfigure(&mut self, bitrate_kbps: u32, dims: Option<(u32, u32)>) -> Result<()>;
}

/// A casting transport (AirPlay mirror, DLNA, Miracast, Cast).
pub trait CastSink: Send {
    /// What this sink can accept; used by core to negotiate quality.
    fn capabilities(&self) -> SinkCaps;
    /// Establish a session with the device.
    fn connect(&mut self, device: &Device) -> Result<()>;
    /// Push one encoded video unit (carries its own PTS).
    fn push_video(&mut self, unit: EncodedUnit) -> Result<()>;
    /// Push one encoded audio unit (carries its own PTS).
    fn push_audio(&mut self, unit: EncodedUnit) -> Result<()>;
    /// Tear down the session.
    fn disconnect(&mut self);
}
