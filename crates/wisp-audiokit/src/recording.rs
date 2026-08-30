//! Rust-side Ogg/Opus recording.
//!
//! Captured interleaved `Float32` PCM (resampled to 48 kHz by the Swift
//! capture layer) is accumulated into whole 20 ms (960-sample) frames,
//! encoded with [`shiguredo_opus`], and each Opus packet is emitted as its own
//! Ogg page so a crash loses at most the packet currently being encoded. A
//! missing EOS flag does not invalidate the pages already present in the file.
//!
//! This replaces the previous Swift recorder (`OpusOggRecorder` +
//! `OggOpusWriter`). Its page framing, granule accounting, pre-skip handling,
//! and EOS/truncated-close semantics are ported faithfully so the produced
//! `.ogg` files remain byte-for-byte equivalent for the same input PCM.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Seek, SeekFrom, Write};
use std::path::Path;

use shiguredo_opus::{Encoder, EncoderConfig};
use wisp_core::{AudioFrame, SourceKind, TrackId};

/// Opus output sample rate. Encoder input and the Ogg granule timebase both
/// use the mandatory 48 kHz Opus-in-Ogg mapping (RFC 7845).
pub(crate) const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Encoder frame length in samples per channel (20 ms at 48 kHz).
const FRAME_SAMPLES: usize = 960;

/// One Opus page per audio packet. The previous Swift writer synced about
/// once per second of audio (50 × 20 ms packets) for a bounded crash
/// durability window; the number is kept identical here.
const PAGES_BETWEEN_SYNCS: u32 = 50;

/// Vendor string written into the `OpusTags` comment header.
const VENDOR: &[u8] = b"WispAudioKit";

/// Errors surfaced while encoding and muxing a recording.
#[derive(Debug)]
pub(crate) enum RecorderError {
    /// The output file or reservation could not be created or written.
    Io(io::Error),
    /// Opus encoding failed.
    Opus(String),
    /// Capture presented a channel count the codec cannot handle.
    UnsupportedChannels(u16),
    /// A call was made out of lifecycle order (after close, etc.).
    InvalidState(&'static str),
}

impl fmt::Display for RecorderError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "ogg recording i/o: {error}"),
            Self::Opus(message) => write!(f, "ogg recording opus: {message}"),
            Self::UnsupportedChannels(channels) => {
                write!(f, "unsupported channel count: {channels}")
            },
            Self::InvalidState(phase) => write!(f, "recorder used out of sequence: {phase}"),
        }
    }
}

impl std::error::Error for RecorderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Opus(_) | Self::UnsupportedChannels(_) | Self::InvalidState(_) => None,
        }
    }
}

impl From<io::Error> for RecorderError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

type Result<T> = std::result::Result<T, RecorderError>;

/// Parses an Opus packet's decoded duration in samples in the 48 kHz Ogg
/// granule timebase. Ported from the previous Swift recorder (RFC 6716,
/// section 3.1).
fn opus_packet_sample_count(packet: &[u8]) -> u64 {
    let Some(&toc) = packet.first() else {
        return 0;
    };
    let config = u64::from(toc >> 3);
    let samples_per_frame = if config >= 16 {
        120_u64 << (config & 0x03)
    } else if config >= 12 {
        480_u64 << (config & 0x01)
    } else if config & 0x03 == 0x03 {
        2880
    } else {
        480_u64 << (config & 0x03)
    };

    let frame_code = toc & 0x03;
    let frame_count: u64 = match frame_code {
        0 => 1,
        1 | 2 => 2,
        _ if packet.len() > 1 => u64::from(packet[1] & 0x3F),
        _ => 0,
    };
    samples_per_frame.saturating_mul(frame_count).min(5760)
}

/// Appends an Ogg-ready 4-byte little-endian integer to `bytes`.
fn extend_u32_le(
    bytes: &mut Vec<u8>,
    value: u32,
) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

/// Appends an Ogg-ready 8-byte little-endian integer to `bytes`.
fn extend_u64_le(
    bytes: &mut Vec<u8>,
    value: u64,
) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

/// Computes the Ogg `CRC-32` (poly `0x04C1_1DB7`, init 0, no final XOR) over a
/// page whose checksum field is zeroed.
fn ogg_crc(page: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in page {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The last audio page written, kept so finish can rewrite it in place with
/// an EOS flag and a trimmed final granule.
#[derive(Clone)]
struct LastAudioPage {
    offset: u64,
    sequence: u32,
    packet: Vec<u8>,
    start_granule: u64,
    end_granule: u64,
}

/// Writes one continuously playable Ogg logical stream to a file. Ported
/// from the Swift `OggOpusWriter`.
struct OggOpusWriter {
    file: File,
    serial_number: u32,
    sequence_number: u32,
    encoded_granule: u64,
    bytes_written: u64,
    audio_pages_since_sync: u32,
    last_audio_page: Option<LastAudioPage>,
    is_closed: bool,
}

impl OggOpusWriter {
    fn create(
        path: &Path,
        channel_count: u8,
        pre_skip: u16,
    ) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(RecorderError::from)?;
        let mut writer = Self {
            file,
            serial_number: fastrand_u32(),
            sequence_number: 0,
            encoded_granule: 0,
            bytes_written: 0,
            audio_pages_since_sync: 0,
            last_audio_page: None,
            is_closed: false,
        };

        let mut head = Vec::from("OpusHead".as_bytes());
        head.push(1); // version
        head.push(channel_count);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        extend_u32_le(&mut head, OPUS_SAMPLE_RATE);
        extend_u32_le(&mut head, 0); // output gain
        head.push(0); // channel mapping family 0 (mono/stereo)
        writer.write_page(&head, 0, 0x02)?; // BOS

        let mut tags = Vec::from("OpusTags".as_bytes());
        extend_u32_le(&mut tags, u32::try_from(VENDOR.len()).unwrap_or(0));
        tags.extend_from_slice(VENDOR);
        extend_u32_le(&mut tags, 0); // user comment count
        writer.write_page(&tags, 0, 0)?;
        writer.file.sync_all()?;
        Ok(writer)
    }

    fn write_audio_packet(
        &mut self,
        packet: &[u8],
    ) -> Result<()> {
        if self.is_closed || packet.is_empty() {
            return Ok(());
        }
        let start_granule = self.encoded_granule;
        self.encoded_granule = self
            .encoded_granule
            .saturating_add(opus_packet_sample_count(packet));
        let (offset, sequence) = self.write_page(packet, self.encoded_granule, 0)?;
        self.last_audio_page = Some(LastAudioPage {
            offset,
            sequence,
            packet: packet.to_vec(),
            start_granule,
            end_granule: self.encoded_granule,
        });
        self.audio_pages_since_sync += 1;
        if self.audio_pages_since_sync >= PAGES_BETWEEN_SYNCS {
            self.file.sync_data()?;
            self.audio_pages_since_sync = 0;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        final_granule: u64,
    ) -> Result<()> {
        if self.is_closed {
            return Ok(());
        }
        if let Some(last) = self.last_audio_page.clone() {
            let trimmed = last.end_granule.min(last.start_granule.max(final_granule));
            let eos_page = Self::make_page(
                &last.packet,
                trimmed,
                0x04, // EOS
                last.sequence,
                self.serial_number,
            )?;
            self.file.seek(SeekFrom::Start(last.offset))?;
            self.file.write_all(&eos_page)?;
        } else {
            let empty = Self::make_page(&[], 0, 0x04, self.sequence_number, self.serial_number)?;
            self.file.write_all(&empty)?;
        }
        self.file.sync_all()?;
        self.is_closed = true;
        Ok(())
    }

    /// Closes without manufacturing an EOS. Already-completed pages remain
    /// usable as a truncated recording.
    fn close_truncated(&mut self) -> Result<()> {
        if self.is_closed {
            return Ok(());
        }
        self.file.sync_all()?;
        self.is_closed = true;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn make_page(
        packet: &[u8],
        granule: u64,
        flags: u8,
        sequence: u32,
        serial_number: u32,
    ) -> Result<Vec<u8>> {
        let quotient = packet.len() / 255;
        let remainder = packet.len() % 255;
        let segment_count = quotient + 1;
        if segment_count > 255 {
            return Err(RecorderError::InvalidState(
                "packet cannot fit on one Ogg page",
            ));
        }

        let mut page = Vec::with_capacity(27 + segment_count + packet.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // stream structure version
        page.push(flags);
        extend_u64_le(&mut page, granule);
        extend_u32_le(&mut page, serial_number);
        extend_u32_le(&mut page, sequence);
        extend_u32_le(&mut page, 0); // checksum placeholder
        page.push(u8::try_from(segment_count).unwrap_or(u8::MAX));
        if quotient > 0 {
            page.extend(std::iter::repeat_n(u8::MAX, quotient));
        }
        page.push(u8::try_from(remainder).unwrap_or(0));
        page.extend_from_slice(packet);

        let checksum = ogg_crc(&page);
        page[22..26].copy_from_slice(&checksum.to_le_bytes());
        Ok(page)
    }

    fn write_page(
        &mut self,
        packet: &[u8],
        granule: u64,
        flags: u8,
    ) -> Result<(u64, u32)> {
        if self.is_closed {
            return Err(RecorderError::InvalidState("writer is closed"));
        }
        let offset = self.bytes_written;
        let sequence = self.sequence_number;
        let page = Self::make_page(packet, granule, flags, sequence, self.serial_number)?;
        self.file.write_all(&page)?;
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(page.len()).unwrap_or(u64::MAX));
        self.sequence_number = self.sequence_number.wrapping_add(1);
        Ok((offset, sequence))
    }
}

impl Drop for OggOpusWriter {
    fn drop(&mut self) {
        if !self.is_closed {
            let _ = self.file.sync_all();
        }
    }
}

/// A non-cryptographic random 32-bit value for an Ogg stream serial number.
fn fastrand_u32() -> u32 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = u32::try_from(duration.as_secs() % (u64::from(u32::MAX) + 1)).unwrap_or(0);
    secs.wrapping_add(duration.subsec_nanos())
}

/// A single audio source's encoder + Ogg writer + frame accumulator.
struct SourceRecorder {
    channels: u8,
    pre_skip: u64,
    encoder: Encoder,
    writer: OggOpusWriter,
    /// Interleaved f32 samples awaiting a whole encoder frame.
    pending: Vec<f32>,
    /// Total input frames (per channel) fed to the accumulator.
    total_input_frames: u64,
    is_closed: bool,
}

impl SourceRecorder {
    fn create(
        path: &Path,
        channels: u16,
    ) -> Result<Self> {
        // Opus only supports mono/stereo; reject anything larger before the
        // narrowing to the encoder's `u8` channel argument.
        if !matches!(channels, 1 | 2) {
            return Err(RecorderError::UnsupportedChannels(channels));
        }
        let channels = if channels == 1 { 1 } else { 2 };
        let encoder = Encoder::new(EncoderConfig {
            bitrate: Some(32_000 * u32::from(channels)),
            ..EncoderConfig::new(OPUS_SAMPLE_RATE, channels)
        })
        .map_err(|error| RecorderError::Opus(error.to_string()))?;
        let pre_skip: u16 = encoder
            .get_lookahead()
            .map_err(|error| RecorderError::Opus(error.to_string()))?;
        let writer = OggOpusWriter::create(path, channels, pre_skip)?;
        Ok(Self {
            channels,
            pre_skip: u64::from(pre_skip),
            encoder,
            writer,
            pending: Vec::new(),
            total_input_frames: 0,
            is_closed: false,
        })
    }

    fn push(
        &mut self,
        samples: &[f32],
        frame_channels: u16,
    ) -> Result<()> {
        if self.is_closed {
            return Err(RecorderError::InvalidState("recorder is closed"));
        }
        if u16::from(self.channels) != frame_channels {
            return Err(RecorderError::InvalidState(
                "channel count changed mid-recording",
            ));
        }
        let input_frames =
            u64::try_from(samples.len()).unwrap_or(u64::MAX) / u64::from(frame_channels);
        self.total_input_frames = self.total_input_frames.saturating_add(input_frames);
        self.pending.extend_from_slice(samples);
        let frame_len = FRAME_SAMPLES * usize::from(self.channels);
        while self.pending.len() >= frame_len {
            let frame: Vec<f32> = self.pending.drain(..frame_len).collect();
            let packet = self
                .encoder
                .encode_f32(&frame)
                .map_err(|error| RecorderError::Opus(error.to_string()))?;
            self.writer.write_audio_packet(&packet)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.is_closed {
            return Ok(());
        }
        let frame_len = FRAME_SAMPLES * usize::from(self.channels);
        if !self.pending.is_empty() {
            // Pad the trailing partial frame to a full 20 ms frame. The final
            // granule is recomputed from the actual input length so padding
            // does not over-report audio duration.
            self.pending.resize(frame_len, 0.0);
            let frame = std::mem::take(&mut self.pending);
            let packet = self
                .encoder
                .encode_f32(&frame)
                .map_err(|error| RecorderError::Opus(error.to_string()))?;
            self.writer.write_audio_packet(&packet)?;
        }
        let final_granule = self.pre_skip.saturating_add(self.total_input_frames);
        self.writer.finish(final_granule)?;
        self.is_closed = true;
        Ok(())
    }

    fn abort(&mut self) -> Result<()> {
        if self.is_closed {
            return Ok(());
        }
        self.writer.close_truncated()?;
        self.is_closed = true;
        Ok(())
    }
}

/// Which audio source a recorder track belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecorderSource {
    Mic,
    System,
}

impl RecorderSource {
    fn file_name(self) -> &'static str {
        match self {
            Self::Mic => "mic.ogg",
            Self::System => "system.ogg",
        }
    }
}

/// Owns the mic and system Ogg/Opus recorders for one session.
///
/// Each source recorder is created lazily when its first audio frame arrives,
/// because the channel count (and even the presence of system audio) is only
/// known once capture begins. The output directory was already reserved by the
/// Swift session (which refuses a reused/partial directory), so the files are
/// always created fresh here.
pub(crate) struct OggOpusRecorder {
    output_dir: std::path::PathBuf,
    mic: Option<SourceRecorder>,
    system: Option<SourceRecorder>,
    is_finished: bool,
}

impl OggOpusRecorder {
    /// New recorder writing `output_dir/mic.ogg` and `output_dir/system.ogg`.
    ///
    /// # Errors
    /// Returns when the output directory is not usable.
    pub(crate) fn new(output_dir: &Path) -> Result<Self> {
        if !output_dir.is_dir() {
            return Err(RecorderError::Io(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "recording output directory not found: {}",
                    output_dir.display()
                ),
            )));
        }
        Ok(Self {
            output_dir: output_dir.to_path_buf(),
            mic: None,
            system: None,
            is_finished: false,
        })
    }

    /// Routes one captured frame to its source recorder, creating the source
    /// recorder from the frame's channel count if this is its first frame.
    ///
    /// # Errors
    /// Returns on encode/write failure; the backing file is always left in a
    /// recoverable truncated state.
    pub(crate) fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> Result<()> {
        let Some(samples) = frame.samples().as_f32() else {
            return Ok(());
        };
        let Some(source) = recorder_source(frame) else {
            return Ok(());
        };
        let frame_channels = frame.format().channels;
        let recorder = self.ensure_source(source, frame_channels)?;
        recorder.push(samples, frame_channels)
    }

    fn ensure_source(
        &mut self,
        source: RecorderSource,
        channels: u16,
    ) -> Result<&mut SourceRecorder> {
        let slot = match source {
            RecorderSource::Mic => &mut self.mic,
            RecorderSource::System => &mut self.system,
        };
        if slot.is_none() {
            let path = self.output_dir.join(source.file_name());
            *slot = Some(SourceRecorder::create(&path, channels)?);
        }
        // `slot` is guaranteed populated (it was either already present or just
        // created above); map the impossible branch to a recoverable error so
        // construction failures never fall through to a panic.
        match slot {
            Some(recorder) => Ok(recorder),
            None => Err(RecorderError::InvalidState(
                "source recorder is unavailable",
            )),
        }
    }

    /// Flushes trailing samples, writes EOS, and closes both files.
    ///
    /// # Errors
    /// Returns when either created source fails to finalize.
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.is_finished {
            return Ok(());
        }
        self.is_finished = true;
        self.mic.as_mut().map_or(Ok(()), SourceRecorder::finish)?;
        self.system.as_mut().map_or(Ok(()), SourceRecorder::finish)
    }

    /// Closes created files without manufacturing EOS; completed pages remain
    /// usable as truncated recordings.
    ///
    /// # Errors
    /// Returns when either created source fails to close.
    pub(crate) fn abort(&mut self) -> Result<()> {
        if self.is_finished {
            return Ok(());
        }
        self.is_finished = true;
        let mic_result = self.mic.as_mut().map_or(Ok(()), SourceRecorder::abort);
        let system_result = self.system.as_mut().map_or(Ok(()), SourceRecorder::abort);
        mic_result.and(system_result)
    }
}

/// Maps a captured frame to its recorder track, if it belongs to a recorded
/// source.
fn recorder_source(frame: &AudioFrame) -> Option<RecorderSource> {
    match (frame.track_id(), frame.source()) {
        (TrackId::MICROPHONE, SourceKind::Microphone) => Some(RecorderSource::Mic),
        (TrackId::SYSTEM, SourceKind::SystemAudio) => Some(RecorderSource::System),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_pages(data: &[u8]) -> Vec<(u8, u64, u32, Vec<u8>)> {
        let mut pages = Vec::new();
        let mut offset = 0;
        while offset + 27 <= data.len() && &data[offset..offset + 4] == b"OggS" {
            let flags = data[offset + 5];
            let granule = u64::from_le_bytes(data[offset + 6..offset + 14].try_into().unwrap());
            let sequence = u32::from_le_bytes(data[offset + 18..offset + 22].try_into().unwrap());
            let segment_count = usize::from(data[offset + 26]);
            let body_start = offset + 27;
            let body_len: usize = data[body_start..body_start + segment_count]
                .iter()
                .map(|&n| usize::from(n))
                .sum();
            let packet =
                data[body_start + segment_count..body_start + segment_count + body_len].to_vec();
            pages.push((flags, granule, sequence, packet));
            offset = body_start + segment_count + body_len;
        }
        pages
    }

    #[test]
    fn writer_emits_headers_then_audio_pages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.ogg");
        let mut writer = OggOpusWriter::create(&path, 1, 312).expect("create");
        // Two audio packets.
        let silence = vec![0u8; 32];
        writer.write_audio_packet(&silence).expect("packet");
        writer.write_audio_packet(&silence).expect("packet");
        writer.finish(312 + 960 * 2).expect("finish");

        let data = std::fs::read(&path).expect("read");
        let pages = parse_pages(&data);
        assert_eq!(pages.len(), 4);
        assert_eq!(pages[0].0, 0x02); // BOS
        assert_eq!(&pages[0].3[..8], b"OpusHead");
        assert_eq!(&pages[1].3[..8], b"OpusTags");
        assert_eq!(pages[2].0, 0x00);
        assert_eq!(pages[3].0, 0x04); // EOS
        // Sequence numbers are contiguous from 0.
        let seqs: Vec<u32> = pages.iter().map(|p| p.2).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        // Every page CRC is valid.
        assert!(page_has_valid_crc(&data));
    }

    fn page_has_valid_crc(data: &[u8]) -> bool {
        // Re-parse and verify: copy each page, zero its checksum, compare.
        let mut offset = 0;
        let mut valid = true;
        while offset + 27 <= data.len() && &data[offset..offset + 4] == b"OggS" {
            let segment_count = usize::from(data[offset + 26]);
            let body_start = offset + 27;
            let body_len: usize = data[body_start..body_start + segment_count]
                .iter()
                .map(|&n| usize::from(n))
                .sum();
            let end = body_start + segment_count + body_len;
            let mut page = data[offset..end].to_vec();
            let expected = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].copy_from_slice(&[0, 0, 0, 0]);
            valid &= ogg_crc(&page) == expected;
            offset = end;
        }
        valid
    }

    #[test]
    fn header_contains_preskip_and_48k_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.ogg");
        let mut writer = OggOpusWriter::create(&path, 2, 312).expect("create");
        let silence = vec![0u8; 32];
        writer.write_audio_packet(&silence).expect("packet");
        writer.finish(312 + 960).expect("finish");
        let data = std::fs::read(&path).expect("read");
        // OpusHead layout: "OpusHead" + version + channels + preskip(LE16) + rate(LE32).
        let body_start = 27;
        let seg = usize::from(data[26]);
        let head_start = body_start + seg;
        assert_eq!(&data[head_start..head_start + 8], b"OpusHead");
        assert_eq!(data[head_start + 8], 1); // version
        assert_eq!(data[head_start + 9], 2); // channels
        assert_eq!(
            u16::from_le_bytes(data[head_start + 10..head_start + 12].try_into().unwrap()),
            312
        );
        assert_eq!(
            u32::from_le_bytes(data[head_start + 12..head_start + 16].try_into().unwrap()),
            48_000
        );
    }

    #[test]
    fn truncated_close_writes_no_eos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.ogg");
        let mut writer = OggOpusWriter::create(&path, 1, 312).expect("create");
        let silence = vec![0u8; 32];
        writer.write_audio_packet(&silence).expect("packet");
        writer.close_truncated().expect("close");
        let data = std::fs::read(&path).expect("read");
        let pages = parse_pages(&data);
        assert_eq!(pages.len(), 3); // BOS + tags + one audio packet
        assert_ne!(pages[2].0, 0x04);
    }
}
