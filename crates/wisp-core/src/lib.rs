//! Shared types and primitives used across Wisp crates.
//!
//! Kept platform-agnostic so the same types flow from the Swift audio
//! framework wrapper (`wisp-audiokit`) into storage (`wisp-storage`) and
//! the `GPUI` desktop app (`wisp-desktop`).

mod audio;
mod error;
mod ids;
mod source;
mod transcript;

pub use audio::{
    AudioFormat, AudioFrame, AudioFrameError, AudioSamples, CaptureEvent, MonotonicTimestamp,
    SampleFormat, SourceKind, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment,
    TranscriptSegmentId,
};
pub use error::SourceLabelError;
pub use ids::{SegmentId, SessionId};
pub use source::SourceLabel;
pub use transcript::{NewSegment, NewSession, Segment, Session};
