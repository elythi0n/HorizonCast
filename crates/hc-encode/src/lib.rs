//! `hc-encode` — hardware H.264 video encoding.
//!
//! Takes [`hc_capture::CapturedFrame`]s (NV12) and produces Annex-B H.264 access units
//! ready for `hc_net::mpegts::MpegTsMuxer`. Windows uses Media Foundation. macOS uses
//! VideoToolbox, but only when the `videotoolbox` feature is enabled — it's a draft that
//! must be finished/validated on a real Mac, so by default macOS uses the stub (and the
//! desktop app there still does file/URL casting; only live mirror is unavailable). Linux
//! (VAAPI) is a stub for now.

#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
mod macos;

#[cfg(target_os = "windows")]
mod mediafoundation;

use hc_capture::CapturedFrame;
use hc_core::Result;

/// Low-latency H.264 encoder configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Target frame rate.
    pub fps: u32,
    /// Target average bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Keyframe (IDR) interval in frames. Smaller = faster join / loss recovery on the TV.
    pub keyframe_interval: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 12_000,
            keyframe_interval: 120, // ~2s at 60fps
        }
    }
}

/// One encoded H.264 access unit (Annex-B, with start codes), carrying its 90 kHz PTS and
/// keyframe flag — feed straight to `MpegTsMuxer::push_access_unit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Annex-B bytes (SPS/PPS prepended on keyframes).
    pub data: Vec<u8>,
    /// 90 kHz presentation timestamp.
    pub pts: u64,
    /// Whether this access unit is an IDR keyframe.
    pub keyframe: bool,
}

/// A hardware H.264 encoder (Media Foundation on Windows; VideoToolbox on macOS when the
/// `videotoolbox` feature is enabled).
pub struct H264Encoder {
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    inner: macos::VtEncoder,
    #[cfg(target_os = "windows")]
    inner: mediafoundation::MfEncoder,
}

impl H264Encoder {
    /// Create an encoder for the given configuration.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
        {
            Ok(Self {
                inner: macos::VtEncoder::new(config)?,
            })
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self {
                inner: mediafoundation::MfEncoder::new(config)?,
            })
        }
        #[cfg(not(any(
            all(target_os = "macos", feature = "videotoolbox"),
            target_os = "windows"
        )))]
        {
            let _ = config;
            Err(hc_core::Error::Unsupported(
                "no H.264 encoding backend is built for this platform".into(),
            ))
        }
    }

    /// Encode one captured frame at the given 90 kHz `pts`. Returns the access units the
    /// encoder emitted (low-latency config emits ~one per frame, but it may return zero
    /// while priming or several when flushing).
    pub fn encode(&mut self, frame: &CapturedFrame, pts: u64) -> Result<Vec<EncodedFrame>> {
        #[cfg(any(
            all(target_os = "macos", feature = "videotoolbox"),
            target_os = "windows"
        ))]
        {
            self.inner.encode(frame, pts)
        }
        #[cfg(not(any(
            all(target_os = "macos", feature = "videotoolbox"),
            target_os = "windows"
        )))]
        {
            let _ = (frame, pts);
            Ok(Vec::new())
        }
    }

    /// Flush any buffered access units at end of stream.
    pub fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        #[cfg(any(
            all(target_os = "macos", feature = "videotoolbox"),
            target_os = "windows"
        ))]
        {
            self.inner.finish()
        }
        #[cfg(not(any(
            all(target_os = "macos", feature = "videotoolbox"),
            target_os = "windows"
        )))]
        {
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_frame_holds_data() {
        let f = EncodedFrame {
            data: vec![0, 0, 0, 1, 0x65],
            pts: 90_000,
            keyframe: true,
        };
        assert!(f.keyframe);
        assert_eq!(f.pts, 90_000);
    }

    #[test]
    fn default_config_is_low_latency_1080p60() {
        let c = EncoderConfig::default();
        assert_eq!((c.width, c.height, c.fps), (1920, 1080, 60));
        assert!(c.keyframe_interval > 0);
    }

    // Platforms without a compiled-in backend report encoding unsupported (Linux, and
    // macOS unless the `videotoolbox` feature is enabled).
    #[cfg(not(any(
        all(target_os = "macos", feature = "videotoolbox"),
        target_os = "windows"
    )))]
    #[test]
    fn new_is_unsupported_without_a_backend() {
        assert!(
            H264Encoder::new(EncoderConfig::default()).is_err(),
            "platforms without an encoder backend report it unsupported"
        );
    }
}
