//! Provider-neutral PCM conversion and streaming resampling helpers.

use wisp_core::{AudioFrame, AudioSamples};

pub(crate) fn pcm_to_mono_samples(frame: &AudioFrame) -> Result<Vec<f32>, wisp_core::SampleFormat> {
    let channels = frame.format().channels;
    let mono = match frame.samples() {
        AudioSamples::F32(samples) => downmix(samples.iter().copied(), channels),
        AudioSamples::I16(samples) => downmix(
            samples.iter().map(|sample| f32::from(*sample) / 32_768.0),
            channels,
        ),
        _ => return Err(frame.format().sample_format.clone()),
    };
    Ok(mono)
}

fn downmix(
    samples: impl Iterator<Item = f32>,
    channels: u16,
) -> Vec<f32> {
    let samples = samples.collect::<Vec<_>>();
    samples
        .chunks_exact(usize::from(channels))
        .map(|frame| frame.iter().sum::<f32>() / f32::from(channels))
        .collect()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub(crate) struct StreamingResampler {
    pub(crate) source_rate: u32,
    target_rate: u32,
    position: f64,
    previous: Option<f32>,
}

impl StreamingResampler {
    pub(crate) const fn new(
        source_rate: u32,
        target_rate: u32,
    ) -> Self {
        Self {
            source_rate,
            target_rate,
            position: 0.0,
            previous: None,
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    pub(crate) fn push(
        &mut self,
        samples: &[f32],
    ) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        if self.source_rate == self.target_rate {
            return samples.to_vec();
        }
        let mut joined = Vec::with_capacity(samples.len() + usize::from(self.previous.is_some()));
        if let Some(previous) = self.previous {
            joined.push(previous);
        }
        joined.extend_from_slice(samples);
        let mut output = Vec::new();
        let step = f64::from(self.source_rate) / f64::from(self.target_rate);
        while self.position + 1.0 < joined.len() as f64 {
            let left = self.position.floor() as usize;
            let fraction = (self.position - left as f64) as f32;
            output.push(joined[left].mul_add(1.0 - fraction, joined[left + 1] * fraction));
            self.position += step;
        }
        self.position -= (joined.len() - 1) as f64;
        self.previous = joined.last().copied();
        output
    }
}

#[cfg(test)]
mod tests {
    use wisp_core::{
        AudioFormat, AudioFrame, AudioSamples, MonotonicTimestamp, SampleFormat, SourceKind,
        TrackId,
    };

    use super::{StreamingResampler, pcm_to_mono_samples};

    #[test]
    fn resampler_changes_rate_and_preserves_edges() {
        let mut resampler = StreamingResampler::new(4, 8);
        let mut output = resampler.push(&[0.0, 1.0]);
        output.extend(resampler.push(&[0.0, -1.0]));
        assert_eq!(output.len(), 6);
        assert!(output[0].abs() < f32::EPSILON);
        assert!(output.iter().all(|sample| (-1.0..=1.0).contains(sample)));
    }

    #[test]
    fn resampler_is_identical_across_frame_boundaries() {
        let samples = [0.0, 0.25, 1.0, -0.5, -1.0, 0.75];
        let mut whole = StreamingResampler::new(44_100, 16_000);
        let expected = whole.push(&samples);
        let mut split = StreamingResampler::new(44_100, 16_000);
        let mut actual = split.push(&samples[..2]);
        actual.extend(split.push(&samples[2..4]));
        actual.extend(split.push(&samples[4..]));
        assert_eq!(actual, expected);
    }

    #[test]
    fn pcm_conversion_supports_f32_and_i16_and_rejects_other_formats() {
        let f32_frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            16_000,
            2,
            vec![1.0, -1.0, 0.5, 0.5],
        )
        .expect("f32 frame");
        assert_eq!(pcm_to_mono_samples(&f32_frame).unwrap(), [0.0, 0.5]);

        let i16_frame = AudioFrame::try_new(
            TrackId::SYSTEM,
            SourceKind::SystemAudio,
            0,
            MonotonicTimestamp::default(),
            AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                sample_format: SampleFormat::I16,
            },
            2,
            AudioSamples::I16(vec![i16::MIN, i16::MAX]),
        )
        .expect("i16 frame");
        let converted = pcm_to_mono_samples(&i16_frame).unwrap();
        assert!((converted[0] + 1.0).abs() < f32::EPSILON);
        assert!((converted[1] - 0.999_969_5).abs() < 0.000_001);

        let unsupported = AudioFrame::try_new(
            TrackId::SYSTEM,
            SourceKind::SystemAudio,
            1,
            MonotonicTimestamp::default(),
            AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                sample_format: SampleFormat::U16,
            },
            1,
            AudioSamples::U16(vec![0]),
        )
        .expect("u16 frame");
        assert_eq!(
            pcm_to_mono_samples(&unsupported).unwrap_err(),
            SampleFormat::U16
        );
    }
}
