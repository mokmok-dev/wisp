//! Shared types and primitives used across Wisp crates.
//!
//! Kept platform-agnostic so the same types flow from the Swift audio
//! framework wrapper (`wisp-audiokit`) into storage (`wisp-storage`) and
//! the `GPUI` desktop app (`wisp-desktop`).

mod ids;
mod source;
mod transcript;

pub use ids::{SegmentId, SessionId};
pub use source::SourceLabel;
pub use transcript::{NewSegment, NewSession, Segment, Session};
