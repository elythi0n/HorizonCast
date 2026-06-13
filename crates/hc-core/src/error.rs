//! Core error type shared across the pipeline.

use thiserror::Error;

/// Errors surfaced by core operations and backend implementations.
#[derive(Debug, Error)]
pub enum Error {
    /// Screen/audio capture failed.
    #[error("capture error: {0}")]
    Capture(String),

    /// Encoding failed.
    #[error("encode error: {0}")]
    Encode(String),

    /// A cast transport (AirPlay/DLNA/…) failed.
    #[error("sink error: {0}")]
    Sink(String),

    /// Device discovery failed.
    #[error("discovery error: {0}")]
    Discovery(String),

    /// An OS permission (e.g. macOS Screen Recording) was not granted.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The target device could not be reached.
    #[error("device unreachable: {0}")]
    DeviceUnreachable(String),

    /// A protocol-level failure (handshake, packetization, …).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The requested operation is not supported on this platform/device.
    #[error("not supported: {0}")]
    Unsupported(String),

    /// Any other error, preserving the source chain.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience alias for `Result<T, hc_core::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
