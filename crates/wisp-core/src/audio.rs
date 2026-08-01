use std::time::Duration;

/// Stable identifier for one audio track within a recording session.
///
/// The built-in microphone and system tracks keep fixed values for backward
/// compatibility. Platform backends may allocate additional IDs for
/// application/process tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u32);

impl TrackId {
    pub const MICROPHONE: Self = Self(1);
    pub const SYSTEM: Self = Self(2);

    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Semantic kind of an audio source.
///
/// This is deliberately distinct from [`TrackId`]: several application-audio
/// tracks can share the same kind while retaining independent identities.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceKind {
    Microphone,
    SystemAudio,
    ApplicationAudio,
    Loopback,
    Other(String),
}

/// Description published by a capture backend before it starts streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDescriptor {
    pub id: TrackId,
    pub source: SourceKind,
    pub name: String,
}

/// PCM representation used by an [`AudioFrame`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
    Other(String),
}

/// Native PCM format reported by a capture backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl AudioFormat {
    #[must_use]
    pub const fn f32(
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format: SampleFormat::F32,
        }
    }
}

/// Timestamp of the first PCM frame relative to a monotonic origin selected
/// when capture starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonotonicTimestamp(Duration);

impl MonotonicTimestamp {
    #[must_use]
    pub const fn from_duration(value: Duration) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

/// Owned PCM payload. Backends keep their native representation until a
/// consumer-specific conversion stage.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AudioSamples {
    I16(Vec<i16>),
    U16(Vec<u16>),
    I32(Vec<i32>),
    U32(Vec<u32>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bytes(Vec<u8>),
}

impl AudioSamples {
    #[must_use]
    pub const fn len(&self) -> usize {
        match self {
            Self::I16(samples) => samples.len(),
            Self::U16(samples) => samples.len(),
            Self::I32(samples) => samples.len(),
            Self::U32(samples) => samples.len(),
            Self::F32(samples) => samples.len(),
            Self::F64(samples) => samples.len(),
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn as_f32(&self) -> Option<&[f32]> {
        match self {
            Self::F32(samples) => Some(samples),
            _ => None,
        }
    }

    #[must_use]
    pub const fn sample_format(&self) -> Option<SampleFormat> {
        match self {
            Self::I16(_) => Some(SampleFormat::I16),
            Self::U16(_) => Some(SampleFormat::U16),
            Self::I32(_) => Some(SampleFormat::I32),
            Self::U32(_) => Some(SampleFormat::U32),
            Self::F32(_) => Some(SampleFormat::F32),
            Self::F64(_) => Some(SampleFormat::F64),
            Self::Bytes(_) => None,
        }
    }
}

/// One captured PCM chunk with enough metadata to preserve device-native
/// formats, detect discontinuities, and align independent tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    track_id: TrackId,
    source: SourceKind,
    sequence: u64,
    timestamp: MonotonicTimestamp,
    format: AudioFormat,
    frame_count: u32,
    samples: AudioSamples,
}

impl AudioFrame {
    /// Build a validated audio frame.
    ///
    /// Typed payloads contain one sample per channel for every PCM frame.
    /// Opaque byte payloads are accepted only with [`SampleFormat::Other`];
    /// their backend-supplied frame count cannot be inferred from byte length.
    ///
    /// Empty packets are rejected. Capture backends should simply omit them.
    ///
    /// # Errors
    /// Returns [`AudioFrameError`] when format metadata, payload
    /// representation, or frame count disagree.
    pub fn try_new(
        track_id: TrackId,
        source: SourceKind,
        sequence: u64,
        timestamp: MonotonicTimestamp,
        format: AudioFormat,
        frame_count: u32,
        samples: AudioSamples,
    ) -> Result<Self, AudioFrameError> {
        if format.sample_rate == 0 {
            return Err(AudioFrameError::ZeroSampleRate);
        }
        if format.channels == 0 {
            return Err(AudioFrameError::ZeroChannels);
        }
        if frame_count == 0 {
            return Err(AudioFrameError::ZeroFrameCount);
        }
        if samples.is_empty() {
            return Err(AudioFrameError::EmptyPayload);
        }

        match samples.sample_format() {
            Some(actual) if actual != format.sample_format => {
                return Err(AudioFrameError::SampleFormatMismatch {
                    declared: format.sample_format,
                    actual,
                });
            },
            Some(_) => {
                let expected_samples_u64 = u64::from(frame_count) * u64::from(format.channels);
                let expected_samples = usize::try_from(expected_samples_u64)
                    .map_err(|_| AudioFrameError::SampleCountOverflow)?;
                if samples.len() != expected_samples {
                    return Err(AudioFrameError::SampleCountMismatch {
                        samples: samples.len(),
                        expected_samples,
                        frame_count,
                        channels: format.channels,
                    });
                }
            },
            None if !matches!(&format.sample_format, SampleFormat::Other(_)) => {
                return Err(AudioFrameError::OpaquePayloadRequiresOtherFormat);
            },
            None => {},
        }

        Ok(Self {
            track_id,
            source,
            sequence,
            timestamp,
            format,
            frame_count,
            samples,
        })
    }

    /// Build an `f32` frame and derive its per-channel frame count.
    ///
    /// # Errors
    /// Returns [`AudioFrameError`] for a zero-channel format, incomplete
    /// interleaving, frame-count overflow, or any invariant enforced by
    /// [`Self::try_new`] (including zero sample rate, empty payload, and
    /// inconsistent format or frame metadata).
    pub fn from_f32(
        track_id: TrackId,
        source: SourceKind,
        sequence: u64,
        timestamp: MonotonicTimestamp,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) -> Result<Self, AudioFrameError> {
        if channels == 0 {
            return Err(AudioFrameError::ZeroChannels);
        }
        let channels_usize = usize::from(channels);
        if !samples.len().is_multiple_of(channels_usize) {
            return Err(AudioFrameError::IncompleteFrame {
                samples: samples.len(),
                channels,
            });
        }
        let frame_count = samples.len() / channels_usize;
        let frame_count =
            u32::try_from(frame_count).map_err(|_| AudioFrameError::FrameCountOverflow)?;
        Self::try_new(
            track_id,
            source,
            sequence,
            timestamp,
            AudioFormat::f32(sample_rate, channels),
            frame_count,
            AudioSamples::F32(samples),
        )
    }

    #[must_use]
    pub const fn track_id(&self) -> TrackId {
        self.track_id
    }

    #[must_use]
    pub const fn source(&self) -> &SourceKind {
        &self.source
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn timestamp(&self) -> MonotonicTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn format(&self) -> &AudioFormat {
        &self.format
    }

    #[must_use]
    pub const fn frame_count(&self) -> u32 {
        self.frame_count
    }

    #[must_use]
    pub const fn samples(&self) -> &AudioSamples {
        &self.samples
    }

    #[must_use]
    pub fn into_samples(self) -> AudioSamples {
        self.samples
    }
}

/// Invalid metadata supplied while constructing an [`AudioFrame`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioFrameError {
    #[error("audio sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("audio frames must have at least one channel")]
    ZeroChannels,
    #[error("audio packets must contain at least one PCM frame")]
    ZeroFrameCount,
    #[error("audio packets must not contain an empty payload")]
    EmptyPayload,
    #[error("declared sample format {declared:?} does not match payload format {actual:?}")]
    SampleFormatMismatch {
        declared: SampleFormat,
        actual: SampleFormat,
    },
    #[error("opaque byte payloads require SampleFormat::Other")]
    OpaquePayloadRequiresOtherFormat,
    #[error(
        "{samples} samples do not match {expected_samples} expected samples for {frame_count} frames and {channels} channels"
    )]
    SampleCountMismatch {
        samples: usize,
        expected_samples: usize,
        frame_count: u32,
        channels: u16,
    },
    #[error("expected audio sample count exceeds usize")]
    SampleCountOverflow,
    #[error("{samples} samples do not form complete frames for {channels} channels")]
    IncompleteFrame { samples: usize, channels: u16 },
    #[error("audio frame count exceeds u32")]
    FrameCountOverflow,
}

/// Events emitted by a capture backend.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CaptureEvent {
    Samples(AudioFrame),
    /// PCM frames discarded because the bounded consumer queue was full.
    ///
    /// The count uses [`AudioFrame::frame_count`] units, not packet/chunk
    /// count.
    Overflow {
        track_id: TrackId,
        dropped_frames: u64,
    },
    Error {
        track_id: Option<TrackId>,
        message: String,
        recoverable: bool,
    },
}

/// Stable identifier reused by every revision of one transcript segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptSegmentId(u64);

impl TranscriptSegmentId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Backend-neutral transcript segment payload.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSegment {
    pub track_id: TrackId,
    pub segment_id: TranscriptSegmentId,
    pub text: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence_mean: Option<f64>,
    pub confidence_min: Option<f64>,
}

/// A revisable or finalized transcript update.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TranscriptEvent {
    Partial(TranscriptSegment),
    Final(TranscriptSegment),
}

impl TranscriptEvent {
    #[must_use]
    pub const fn segment(&self) -> &TranscriptSegment {
        match self {
            Self::Partial(segment) | Self::Final(segment) => segment,
        }
    }

    #[must_use]
    pub const fn is_final(&self) -> bool {
        matches!(self, Self::Final(_))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioFormat, AudioFrame, AudioFrameError, AudioSamples, MonotonicTimestamp, SampleFormat,
        SourceKind, TrackId, TranscriptEvent, TranscriptSegment, TranscriptSegmentId,
    };

    #[test]
    fn f32_frame_derives_frame_count() {
        let frame = AudioFrame::from_f32(
            TrackId::new(7),
            SourceKind::ApplicationAudio,
            3,
            MonotonicTimestamp::default(),
            48_000,
            2,
            vec![0.0; 10],
        )
        .unwrap();

        assert_eq!(frame.frame_count(), 5);
        assert!(matches!(frame.samples(), AudioSamples::F32(_)));
    }

    #[test]
    fn public_constructor_rejects_inconsistent_metadata() {
        let make = |format, frame_count, samples| {
            AudioFrame::try_new(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                0,
                MonotonicTimestamp::default(),
                format,
                frame_count,
                samples,
            )
        };

        assert_eq!(
            make(AudioFormat::f32(0, 1), 1, AudioSamples::F32(vec![0.0])).unwrap_err(),
            AudioFrameError::ZeroSampleRate
        );
        assert_eq!(
            make(AudioFormat::f32(16_000, 0), 1, AudioSamples::F32(vec![0.0])).unwrap_err(),
            AudioFrameError::ZeroChannels
        );
        assert_eq!(
            make(
                AudioFormat {
                    sample_rate: 16_000,
                    channels: 1,
                    sample_format: SampleFormat::I16,
                },
                1,
                AudioSamples::F32(vec![0.0]),
            )
            .unwrap_err(),
            AudioFrameError::SampleFormatMismatch {
                declared: SampleFormat::I16,
                actual: SampleFormat::F32,
            }
        );
        assert!(matches!(
            make(
                AudioFormat::f32(16_000, 2),
                2,
                AudioSamples::F32(vec![0.0; 2]),
            ),
            Err(AudioFrameError::SampleCountMismatch {
                expected_samples: 4,
                ..
            })
        ));
        assert_eq!(
            make(
                AudioFormat::f32(16_000, 1),
                0,
                AudioSamples::F32(Vec::new())
            )
            .unwrap_err(),
            AudioFrameError::ZeroFrameCount
        );
        assert_eq!(
            make(
                AudioFormat::f32(16_000, 1),
                1,
                AudioSamples::F32(Vec::new())
            )
            .unwrap_err(),
            AudioFrameError::EmptyPayload
        );
        assert_eq!(
            make(AudioFormat::f32(16_000, 1), 1, AudioSamples::Bytes(vec![1])).unwrap_err(),
            AudioFrameError::OpaquePayloadRequiresOtherFormat
        );
    }

    #[test]
    fn f32_frame_rejects_partial_interleaved_frame() {
        let error = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            48_000,
            2,
            vec![0.0; 3],
        )
        .unwrap_err();

        assert_eq!(
            error,
            AudioFrameError::IncompleteFrame {
                samples: 3,
                channels: 2,
            }
        );
    }

    #[test]
    fn partial_and_final_keep_the_same_identity() {
        let segment = TranscriptSegment {
            track_id: TrackId::SYSTEM,
            segment_id: TranscriptSegmentId::new(42),
            text: "draft".into(),
            start_seconds: 1.0,
            end_seconds: 2.0,
            confidence_mean: None,
            confidence_min: None,
        };
        let partial = TranscriptEvent::Partial(segment.clone());
        let final_event = TranscriptEvent::Final(TranscriptSegment {
            text: "final".into(),
            ..segment
        });

        assert_eq!(
            partial.segment().segment_id,
            final_event.segment().segment_id
        );
        assert!(!partial.is_final());
        assert!(final_event.is_final());
    }
}
