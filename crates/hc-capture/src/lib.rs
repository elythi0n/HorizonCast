//! `hc-capture` — screen capture.
//!
//! Produces NV12 [`CapturedFrame`]s stamped on a monotonic clock, ready for the encoder.
//! Windows uses the `scap` crate (Windows Graphics Capture), yielding BGRA which we convert
//! with [`bgra_to_nv12`]. macOS also uses `scap` (ScreenCaptureKit, native NV12), but only
//! when the `screencapturekit` feature is enabled: the current `scap`/screencapturekit-sys
//! calls APIs that don't exist on macOS 12/13, so by default macOS uses the stub (the app
//! still does file/URL casting; only live mirror is unavailable). Linux is a stub for now.
//!
//! The BGRA→NV12 [`convert`] path is portable and unit-tested on every platform.

mod convert;
mod frame;
pub use convert::bgra_to_nv12;
pub use frame::{CapturedFrame, Nv12Planes};

#[cfg(any(
    all(target_os = "macos", feature = "screencapturekit"),
    target_os = "windows"
))]
mod scap_backend;

use hc_core::Result;

/// A running screen capture yielding NV12 frames (Windows; macOS with `screencapturekit`).
pub struct ScreenCapture {
    #[cfg(any(
        all(target_os = "macos", feature = "screencapturekit"),
        target_os = "windows"
    ))]
    inner: scap_backend::ScapCapture,
}

impl ScreenCapture {
    /// Start capturing the primary display at approximately `fps` frames per second.
    ///
    /// On macOS this triggers the Screen Recording permission prompt on first use.
    pub fn start(fps: u32) -> Result<Self> {
        #[cfg(any(
            all(target_os = "macos", feature = "screencapturekit"),
            target_os = "windows"
        ))]
        {
            Ok(Self {
                inner: scap_backend::ScapCapture::start(fps)?,
            })
        }
        #[cfg(not(any(
            all(target_os = "macos", feature = "screencapturekit"),
            target_os = "windows"
        )))]
        {
            let _ = fps;
            Err(hc_core::Error::Unsupported(
                "no screen-capture backend is built for this platform".into(),
            ))
        }
    }

    /// Block until the next captured frame; returns `None` once capture has ended.
    /// Call from a blocking context (it blocks).
    pub fn next_frame(&mut self) -> Option<CapturedFrame> {
        #[cfg(any(
            all(target_os = "macos", feature = "screencapturekit"),
            target_os = "windows"
        ))]
        {
            self.inner.next_frame()
        }
        #[cfg(not(any(
            all(target_os = "macos", feature = "screencapturekit"),
            target_os = "windows"
        )))]
        {
            None
        }
    }

    /// Stop capturing and release resources.
    pub fn stop(self) {
        #[cfg(any(
            all(target_os = "macos", feature = "screencapturekit"),
            target_os = "windows"
        ))]
        {
            self.inner.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    // Only used by the stub-platform test below; gated to match so platforms with a backend
    // (which compile that test out) don't see an unused import under `-D warnings`.
    #[cfg(not(any(
        all(target_os = "macos", feature = "screencapturekit"),
        target_os = "windows"
    )))]
    use super::*;

    // Platforms without a compiled-in capture backend report it unsupported (Linux, and
    // macOS unless the `screencapturekit` feature is enabled).
    #[cfg(not(any(
        all(target_os = "macos", feature = "screencapturekit"),
        target_os = "windows"
    )))]
    #[test]
    fn start_is_unsupported_without_a_backend() {
        assert!(
            ScreenCapture::start(30).is_err(),
            "platforms without a capture backend report it unsupported"
        );
    }
}
