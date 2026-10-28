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
    use std::time::Duration;

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

    #[test]
    fn track_ids_expose_their_value() {
        assert_eq!(TrackId::new(42).get(), 42);
        assert_eq!(TrackId::MICROPHONE.get(), 1);
        assert_eq!(TrackId::SYSTEM.get(), 2);
    }

    #[test]
    fn monotonic_timestamp_roundtrips_its_duration() {
        let value = Duration::from_millis(12_345);
        assert_eq!(
            MonotonicTimestamp::from_duration(value).as_duration(),
            value
        );
        assert_eq!(MonotonicTimestamp::default().as_duration(), Duration::ZERO);
    }

    #[test]
    fn audio_format_constructor_sets_f32_sample_format() {
        let format = AudioFormat::f32(48_000, 2);
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_format, SampleFormat::F32);
    }

    #[test]
    fn audio_samples_report_their_shape() {
        let payloads = [
            AudioSamples::I16(vec![1, 2, 3, 4, 5]),
            AudioSamples::U16(vec![1, 2, 3, 4, 5]),
            AudioSamples::I32(vec![1, 2, 3, 4, 5]),
            AudioSamples::U32(vec![1, 2, 3, 4, 5]),
            AudioSamples::F32(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
            AudioSamples::F64(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
            AudioSamples::Bytes(vec![1, 2, 3, 4, 5]),
        ];
        for samples in &payloads {
            assert_eq!(samples.len(), 5);
            assert!(!samples.is_empty());
        }

        let empty = [
            AudioSamples::I16(Vec::new()),
            AudioSamples::U16(Vec::new()),
            AudioSamples::I32(Vec::new()),
            AudioSamples::U32(Vec::new()),
            AudioSamples::F32(Vec::new()),
            AudioSamples::F64(Vec::new()),
            AudioSamples::Bytes(Vec::new()),
        ];
        for samples in &empty {
            assert_eq!(samples.len(), 0);
            assert!(samples.is_empty(), "empty audio must report empty");
        }
    }

    #[test]
    fn as_f32_exposes_only_f32_payloads() {
        let payload = vec![0.25, -3.5, 7.75];
        let samples = AudioSamples::F32(payload.clone());
        assert_eq!(samples.as_f32(), Some(payload.as_slice()));

        let others = [
            AudioSamples::I16(vec![1]),
            AudioSamples::U16(vec![1]),
            AudioSamples::I32(vec![1]),
            AudioSamples::U32(vec![1]),
            AudioSamples::F64(vec![1.0]),
            AudioSamples::Bytes(vec![1]),
        ];
        for other in &others {
            assert_eq!(other.as_f32(), None);
        }
    }

    #[test]
    fn sample_format_matches_the_payload_variant() {
        let cases = [
            (AudioSamples::I16(vec![1]), Some(SampleFormat::I16)),
            (AudioSamples::U16(vec![1]), Some(SampleFormat::U16)),
            (AudioSamples::I32(vec![1]), Some(SampleFormat::I32)),
            (AudioSamples::U32(vec![1]), Some(SampleFormat::U32)),
            (AudioSamples::F32(vec![1.0]), Some(SampleFormat::F32)),
            (AudioSamples::F64(vec![1.0]), Some(SampleFormat::F64)),
            (AudioSamples::Bytes(vec![1]), None),
        ];
        for (samples, expected) in cases {
            assert_eq!(samples.sample_format(), expected);
        }
    }

    #[test]
    fn frame_accessors_report_constructed_metadata() {
        let timestamp = MonotonicTimestamp::from_duration(Duration::from_secs(3));
        let frame = AudioFrame::try_new(
            TrackId::new(9),
            SourceKind::ApplicationAudio,
            42,
            timestamp,
            AudioFormat::f32(48_000, 2),
            5,
            AudioSamples::F32(vec![0.5; 10]),
        )
        .unwrap();

        assert_eq!(frame.track_id().get(), 9);
        assert_eq!(frame.source(), &SourceKind::ApplicationAudio);
        assert_eq!(frame.sequence(), 42);
        assert_eq!(frame.timestamp(), timestamp);
        assert_eq!(frame.format(), &AudioFormat::f32(48_000, 2));
        assert_eq!(frame.frame_count(), 5);
        assert_eq!(frame.samples(), &AudioSamples::F32(vec![0.5; 10]));
        assert_eq!(frame.into_samples(), AudioSamples::F32(vec![0.5; 10]));
    }

    #[test]
    fn opaque_payloads_require_the_other_sample_format() {
        let frame = AudioFrame::try_new(
            TrackId::SYSTEM,
            SourceKind::SystemAudio,
            0,
            MonotonicTimestamp::default(),
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: SampleFormat::Other("s16le-planar".into()),
            },
            3,
            AudioSamples::Bytes(vec![1, 2, 3, 4]),
        )
        .unwrap();

        assert_eq!(frame.frame_count(), 3);
        assert_eq!(frame.samples().len(), 4);
        assert_eq!(frame.samples().sample_format(), None);
        assert_eq!(frame.samples().as_f32(), None);
    }

    #[test]
    fn transcript_segment_ids_expose_their_value() {
        assert_eq!(TranscriptSegmentId::new(42).get(), 42);
    }

    #[test]
    fn transcript_events_expose_their_segment() {
        let segment = TranscriptSegment {
            track_id: TrackId::MICROPHONE,
            segment_id: TranscriptSegmentId::new(7),
            text: "hello".into(),
            start_seconds: 1.5,
            end_seconds: 2.5,
            confidence_mean: Some(0.9),
            confidence_min: Some(0.8),
        };
        let partial = TranscriptEvent::Partial(segment.clone());
        let final_event = TranscriptEvent::Final(segment.clone());

        assert_eq!(partial.segment(), &segment);
        assert_eq!(final_event.segment(), &segment);
        assert!(!partial.is_final());
        assert!(final_event.is_final());
    }
}

#[cfg(test)]
mod property_tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::{
        AudioFormat, AudioFrame, AudioFrameError, AudioSamples, MonotonicTimestamp, SampleFormat,
        SourceKind, TrackId,
    };

    /// A typed payload, its matching declared format, and the channels/frame
    /// count that the payload implies.
    fn typed_payload() -> impl Strategy<Value = (u16, u32, SampleFormat, AudioSamples)> {
        (1u16..=8, 1u32..=16, 0usize..6).prop_map(|(channels, frame_count, variant)| {
            let len = usize::try_from(u64::from(frame_count) * u64::from(channels))
                .expect("test payload length fits usize");
            let samples = match variant {
                0 => AudioSamples::I16(
                    (0..len)
                        .map(|i| i16::try_from(i).expect("test payload fits i16") * 3 - 7)
                        .collect(),
                ),
                1 => AudioSamples::U16(
                    (0..len)
                        .map(|i| u16::try_from(i).expect("test payload fits u16") * 3 + 1)
                        .collect(),
                ),
                2 => AudioSamples::I32(
                    (0..len)
                        .map(|i| i32::try_from(i).expect("test payload fits i32") * 5 - 3)
                        .collect(),
                ),
                3 => AudioSamples::U32(
                    (0..len)
                        .map(|i| u32::try_from(i).expect("test payload fits u32") * 7 + 2)
                        .collect(),
                ),
                4 => AudioSamples::F32(
                    (0..len)
                        .map(|i| {
                            f32::from(u16::try_from(i).expect("test payload fits u16"))
                                .mul_add(0.5, -3.25)
                        })
                        .collect(),
                ),
                _ => AudioSamples::F64(
                    (0..len)
                        .map(|i| {
                            f64::from(u32::try_from(i).expect("test payload fits u32"))
                                .mul_add(0.25, -2.5)
                        })
                        .collect(),
                ),
            };
            let format = match variant {
                0 => SampleFormat::I16,
                1 => SampleFormat::U16,
                2 => SampleFormat::I32,
                3 => SampleFormat::U32,
                4 => SampleFormat::F32,
                _ => SampleFormat::F64,
            };
            (channels, frame_count, format, samples)
        })
    }

    proptest! {
        #[test]
        fn consistent_typed_payloads_roundtrip_through_a_frame(
            track in any::<u32>(),
            sequence in any::<u64>(),
            millis in any::<u64>(),
            sample_rate in 1u32..=384_000,
            (channels, frame_count, sample_format, payload) in typed_payload(),
        ) {
            let timestamp = MonotonicTimestamp::from_duration(Duration::from_millis(millis));
            let format = AudioFormat {
                sample_rate,
                channels,
                sample_format: sample_format.clone(),
            };
            let frame = AudioFrame::try_new(
                TrackId::new(track),
                SourceKind::Loopback,
                sequence,
                timestamp,
                format.clone(),
                frame_count,
                payload.clone(),
            )
            .expect("consistent metadata must construct a frame");

            prop_assert_eq!(frame.track_id(), TrackId::new(track));
            prop_assert_eq!(frame.source(), &SourceKind::Loopback);
            prop_assert_eq!(frame.sequence(), sequence);
            prop_assert_eq!(frame.timestamp().as_duration(), Duration::from_millis(millis));
            prop_assert_eq!(frame.format(), &format);
            prop_assert_eq!(frame.frame_count(), frame_count);
            prop_assert_eq!(frame.samples(), &payload);
            prop_assert_eq!(frame.samples().sample_format(), Some(sample_format));
            prop_assert_eq!(frame.into_samples(), payload);
        }

        #[test]
        fn mismatched_sample_and_frame_counts_are_rejected(
            channels in 1u16..=8,
            frame_count in 1u32..=16,
            extra in 1usize..=8,
        ) {
            let expected = usize::try_from(u64::from(frame_count) * u64::from(channels))
                .expect("test payload length fits usize");
            let actual = expected + extra;
            let error = AudioFrame::try_new(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                0,
                MonotonicTimestamp::default(),
                AudioFormat::f32(48_000, channels),
                frame_count,
                AudioSamples::F32(vec![0.0; actual]),
            )
            .expect_err("mismatched payload must be rejected");

            prop_assert_eq!(
                error,
                AudioFrameError::SampleCountMismatch {
                    samples: actual,
                    expected_samples: expected,
                    frame_count,
                    channels,
                }
            );
        }

        #[test]
        fn f32_frames_derive_the_exact_frame_count(
            track in any::<u32>(),
            sequence in any::<u64>(),
            sample_rate in 1u32..=192_000,
            channels in 1u16..=8,
            frame_count in 1u32..=32,
        ) {
            let len = usize::try_from(u64::from(frame_count) * u64::from(channels))
                .expect("test payload length fits usize");
            let samples: Vec<f32> = (0..len)
                .map(|i| {
                    f32::from(u16::try_from(i % 100).expect("index fits u16")).mul_add(0.5, -7.25)
                })
                .collect();
            let frame = AudioFrame::from_f32(
                TrackId::new(track),
                SourceKind::ApplicationAudio,
                sequence,
                MonotonicTimestamp::default(),
                sample_rate,
                channels,
                samples.clone(),
            )
            .expect("interleaved f32 payload must construct a frame");

            prop_assert_eq!(frame.frame_count(), frame_count);
            prop_assert_eq!(frame.samples().as_f32(), Some(samples.as_slice()));
            prop_assert_eq!(frame.format(), &AudioFormat::f32(sample_rate, channels));
        }

        #[test]
        fn opaque_payloads_keep_the_declared_frame_count(
            name in "[a-z0-9-]{1,12}",
            frame_count in 1u32..=16,
            bytes in prop::collection::vec(any::<u8>(), 1..=64),
            channels in 1u16..=8,
            sample_rate in 1u32..=96_000,
        ) {
            let frame = AudioFrame::try_new(
                TrackId::SYSTEM,
                SourceKind::SystemAudio,
                u64::from(frame_count),
                MonotonicTimestamp::default(),
                AudioFormat {
                    sample_rate,
                    channels,
                    sample_format: SampleFormat::Other(name),
                },
                frame_count,
                AudioSamples::Bytes(bytes.clone()),
            )
            .expect("opaque payload with SampleFormat::Other must be accepted");

            prop_assert_eq!(frame.frame_count(), frame_count);
            prop_assert_eq!(frame.samples().len(), bytes.len());
            prop_assert_eq!(frame.samples().sample_format(), None);
            prop_assert_eq!(frame.samples().as_f32(), None);
        }
    }
}
