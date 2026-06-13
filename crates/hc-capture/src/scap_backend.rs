//! Screen capture via the `scap` crate (ScreenCaptureKit on macOS, Windows Graphics
//! Capture on Windows).
//!
//! `scap` runs the OS capture on its own internal thread and hands frames over a channel;
//! `Capturer::get_next_frame` blocks until the next one. We hold the capturer directly and
//! pull from [`crate::ScreenCapture::next_frame`] (always called from one blocking thread,
//! so the capturer never crosses threads — important since it's `!Send` on Windows, where
//! `Options` carries an `HWND`). macOS delivers NV12 directly; Windows delivers BGRA, which
//! we convert with [`crate::bgra_to_nv12`].
//!
//! NOTE: this module is `cfg(any(target_os = "macos", target_os = "windows"))` and is
//! **not** compiled in the Linux dev/CI environment — it is built and validated on a Mac or
//! a Windows machine. The portable BGRA→NV12 conversion path is unit-tested everywhere.

use std::time::Instant;

use hc_core::{Error, Result};
use scap::capturer::{Capturer, Options};
use scap::frame::{Frame, FrameType};

use crate::convert::bgra_to_nv12;
use crate::frame::{CapturedFrame, Nv12Planes};

pub(crate) struct ScapCapture {
    capturer: Capturer,
    /// Monotonic origin for frame timestamps.
    start: Instant,
}

/// macOS hands us NV12 directly; Windows hands us BGRA (we convert).
#[cfg(target_os = "macos")]
const REQUESTED_FRAME_TYPE: FrameType = FrameType::YUVFrame;
#[cfg(target_os = "windows")]
const REQUESTED_FRAME_TYPE: FrameType = FrameType::BGRAFrame;

impl ScapCapture {
    pub(crate) fn start(fps: u32) -> Result<Self> {
        if !scap::is_supported() {
            return Err(Error::Capture(
                "screen capture API not available on this OS version".into(),
            ));
        }
        if !scap::has_permission() && !scap::request_permission() {
            return Err(Error::PermissionDenied(
                "Screen Recording permission was not granted".into(),
            ));
        }

        let options = Options {
            fps,
            show_cursor: true,
            output_type: REQUESTED_FRAME_TYPE,
            ..Default::default()
        };
        let mut capturer = Capturer::build(options)
            .map_err(|e| Error::Capture(format!("could not start screen capture: {e}")))?;
        capturer.start_capture();

        Ok(Self {
            capturer,
            start: Instant::now(),
        })
    }

    pub(crate) fn next_frame(&mut self) -> Option<CapturedFrame> {
        // Skip frame variants we don't handle (we only ever request NV12 or BGRA) rather
        // than guess; loop until we get one we can use or capture ends.
        loop {
            match self.capturer.get_next_frame() {
                Ok(frame) => {
                    if let Some(captured) = to_nv12(frame, self.start) {
                        return Some(captured);
                    }
                }
                Err(_) => return None, // scap's channel closed / capture ended
            }
        }
    }

    pub(crate) fn stop(mut self) {
        self.capturer.stop_capture();
    }
}

/// Convert a `scap` frame to our NV12 [`CapturedFrame`], or `None` for a format we don't
/// handle.
fn to_nv12(frame: Frame, start: Instant) -> Option<CapturedFrame> {
    let pts = start.elapsed();
    match frame {
        // macOS: native NV12.
        Frame::YUVFrame(f) => Some(CapturedFrame {
            width: f.width.max(0) as u32,
            height: f.height.max(0) as u32,
            pts,
            planes: Nv12Planes {
                y: f.luminance_bytes,
                y_stride: f.luminance_stride.max(0) as u32,
                uv: f.chrominance_bytes,
                uv_stride: f.chrominance_stride.max(0) as u32,
            },
        }),
        // Windows: BGRA → NV12. scap's BGRAFrame is tightly packed (stride == width*4).
        Frame::BGRA(f) => {
            let width = f.width.max(0) as u32;
            let height = f.height.max(0) as u32;
            let stride = width * 4;
            Some(CapturedFrame {
                width: width & !1,
                height: height & !1,
                pts,
                planes: bgra_to_nv12(&f.data, width, height, stride),
            })
        }
        _ => None,
    }
}
