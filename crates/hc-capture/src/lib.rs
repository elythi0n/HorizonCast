//! `hc-capture` — screen capture.
//!
//! Produces NV12 [`CapturedFrame`]s stamped on a monotonic clock, ready for the encoder.
//! macOS and Windows are implemented via the `scap` crate (ScreenCaptureKit / Windows
//! Graphics Capture). ScreenCaptureKit yields NV12 directly; WGC yields BGRA, which we
//! convert with [`bgra_to_nv12`]. Linux (PipeWire) is a stub for now. The `scap` backend
//! is `cfg(any(macos, windows))` and is built/validated on those OSes, not in the Linux
//! dev env — but the BGRA→NV12 [`convert`] path is portable and unit-tested everywhere.

mod convert;
mod frame;
pub use convert::bgra_to_nv12;
pub use frame::{CapturedFrame, Nv12Planes};

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod scap_backend;

use hc_core::Result;

/// A running screen capture yielding NV12 frames (macOS + Windows).
pub struct ScreenCapture {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    inner: scap_backend::ScapCapture,
}

impl ScreenCapture {
    /// Start capturing the primary display at approximately `fps` frames per second.
    ///
    /// On macOS this triggers the Screen Recording permission prompt on first use.
    pub fn start(fps: u32) -> Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            Ok(Self {
                inner: scap_backend::ScapCapture::start(fps)?,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = fps;
            Err(hc_core::Error::Unsupported(
                "screen capture is implemented on macOS and Windows (so far)".into(),
            ))
        }
    }

    /// Block until the next captured frame; returns `None` once capture has ended.
    /// Call from a blocking context (it blocks).
    pub fn next_frame(&mut self) -> Option<CapturedFrame> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.inner.next_frame()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }

    /// Stop capturing and release resources.
    pub fn stop(self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.inner.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn start_is_unsupported_on_this_platform() {
        assert!(
            ScreenCapture::start(30).is_err(),
            "platforms without a capture backend report it unsupported (for now)"
        );
    }
}
