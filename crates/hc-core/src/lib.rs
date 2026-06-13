//! `hc-core` — orchestration contracts and session state for HorizonCast.
//!
//! This crate defines the pipeline *contracts* (the [`FrameSource`], [`AudioSource`],
//! [`Encoder`], and [`CastSink`] traits), the shared value [`types`], and the session
//! [`SessionState`] machine. Backend crates (`hc-capture`, `hc-encode`, `hc-sink`)
//! implement the traits; the binaries (`hc-cli`, `hc-app`) wire concrete implementations
//! together. Keeping the contracts here avoids dependency cycles between core and backends.

pub mod clock;
pub mod error;
pub mod session;
pub mod traits;
pub mod types;

pub use clock::PtsClock;
pub use error::{Error, Result};
pub use session::{ErrorReason, Event, SessionState};
pub use traits::{AudioSource, CastSink, Encoder, FrameSource};
pub use types::{
    AudioCodec, AudioFrame, Device, EncodedUnit, Protocol, SinkCaps, Timestamp, VideoCodec,
    VideoConfig, VideoFrame,
};
