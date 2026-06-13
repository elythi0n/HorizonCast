//! Captured-frame types (portable; compiled on all platforms).

use std::time::Duration;

/// NV12 (4:2:0 planar, Y plane + interleaved UV plane) frame data — the format
/// ScreenCaptureKit delivers and hardware H.264 encoders prefer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Planes {
    /// Luminance plane.
    pub y: Vec<u8>,
    /// Bytes per row of the Y plane (may exceed `width` due to alignment padding).
    pub y_stride: u32,
    /// Interleaved Cb/Cr plane (half height).
    pub uv: Vec<u8>,
    /// Bytes per row of the UV plane.
    pub uv_stride: u32,
}

/// One captured video frame, stamped on a monotonic clock measured from capture start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Time since capture started (feed to `hc_core::PtsClock` for a 90 kHz PTS).
    pub pts: Duration,
    /// Pixel data (NV12).
    pub planes: Nv12Planes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_frame_holds_planes() {
        let f = CapturedFrame {
            width: 2,
            height: 2,
            pts: Duration::from_millis(16),
            planes: Nv12Planes {
                y: vec![0; 4],
                y_stride: 2,
                uv: vec![128; 2],
                uv_stride: 2,
            },
        };
        assert_eq!(f.width, 2);
        assert_eq!(f.planes.y.len(), 4);
        assert_eq!(f.planes.uv_stride, 2);
    }
}
