//! Streaming mono Ogg/Opus writer used by the Windows WASAPI backend.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ogg::{PacketWriteEndInfo, PacketWriter};
use ropus::{Application, Bitrate, Channels, Encoder};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const SAMPLE_RATE: u32 = 16_000;
const FRAME_SAMPLES: usize = 320;
const GRANULE_RATE_MULTIPLIER: u32 = 3;
const MAX_OPUS_PACKET_BYTES: usize = 1_275;
const CHANNELS: u8 = 1;
const VENDOR: &str = "Wisp";

static NEXT_SERIAL: AtomicU32 = AtomicU32::new(0x5753_5000);

struct PendingPacket {
    data: Vec<u8>,
    granule_position: u64,
}

/// Incrementally encodes 16 kHz mono `f32` PCM into an Ogg/Opus file.
pub(crate) struct OggOpusRecorder {
    encoder: Encoder,
    writer: Option<PacketWriter<'static, BufWriter<File>>>,
    serial: u32,
    pre_skip: u32,
    pcm: VecDeque<f32>,
    audio_samples: u64,
    encoded_samples: u64,
    pending: Option<PendingPacket>,
    output_dir: PathBuf,
    finished: bool,
}

impl OggOpusRecorder {
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let encoder = Encoder::builder(SAMPLE_RATE, Channels::Mono, Application::Audio)
            .bitrate(Bitrate::Bits(32_000))
            .vbr(true)
            .build()
            .map_err(codec_error)?;
        let input_pre_skip = encoder.lookahead();
        let pre_skip = input_pre_skip
            .checked_mul(GRANULE_RATE_MULTIPLIER)
            .ok_or_else(|| io::Error::other("Opus pre-skip overflow"))?;
        let pre_skip_header = u16::try_from(pre_skip)
            .map_err(|_| io::Error::other(format!("Opus pre-skip {pre_skip} exceeds u16")))?;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        // `create_new` maps to O_CREAT|O_EXCL. In addition to making
        // concurrent recordings collision-safe, POSIX guarantees that an
        // existing final-component symlink is not followed in this mode.
        let file = options.open(path)?;
        let output_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut writer = PacketWriter::new(BufWriter::new(file));
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed) ^ std::process::id();
        writer.write_packet(
            opus_head(pre_skip_header).to_vec(),
            serial,
            PacketWriteEndInfo::EndPage,
            0,
        )?;
        writer.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;

        // Prime the codec so Opus' algorithmic delay is removed by pre-skip
        // without losing the beginning of the caller's audio.
        let input_pre_skip = usize::try_from(input_pre_skip).map_err(io::Error::other)?;
        let pcm = std::iter::repeat_n(0.0, input_pre_skip).collect();

        Ok(Self {
            encoder,
            writer: Some(writer),
            serial,
            pre_skip,
            pcm,
            audio_samples: 0,
            encoded_samples: 0,
            pending: None,
            output_dir,
            finished: false,
        })
    }

    pub(crate) fn push(
        &mut self,
        samples: &[f32],
    ) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("cannot write to a finished Ogg stream"));
        }
        self.audio_samples = self
            .audio_samples
            .checked_add(u64::try_from(samples.len()).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("Ogg/Opus sample count overflow"))?;
        self.pcm.extend(samples.iter().copied());
        self.encode_complete_frames()
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        if !self.pcm.is_empty() || self.pending.is_none() {
            self.pcm.resize(FRAME_SAMPLES, 0.0);
            self.encode_one_frame()?;
        }
        let Some(packet) = self.pending.take() else {
            return Err(io::Error::other("Ogg/Opus stream has no audio packet"));
        };
        let final_granule = u64::from(self.pre_skip)
            .checked_add(
                self.audio_samples
                    .checked_mul(u64::from(GRANULE_RATE_MULTIPLIER))
                    .ok_or_else(|| io::Error::other("Ogg granule position overflow"))?,
            )
            .ok_or_else(|| io::Error::other("Ogg granule position overflow"))?;
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("Ogg writer is already closed"))?;
        writer.write_packet(
            packet.data,
            self.serial,
            PacketWriteEndInfo::EndStream,
            final_granule,
        )?;
        durably_commit(writer.inner_mut())?;
        sync_output_directory(&self.output_dir)?;
        self.finished = true;
        Ok(())
    }

    fn encode_complete_frames(&mut self) -> io::Result<()> {
        while self.pcm.len() >= FRAME_SAMPLES {
            self.encode_one_frame()?;
        }
        Ok(())
    }

    fn encode_one_frame(&mut self) -> io::Result<()> {
        let frame = self.pcm.drain(..FRAME_SAMPLES).collect::<Vec<_>>();
        let mut output = vec![0; MAX_OPUS_PACKET_BYTES];
        let packet_bytes = self
            .encoder
            .encode_float(&frame, &mut output)
            .map_err(codec_error)?;
        output.truncate(packet_bytes);
        self.encoded_samples = self
            .encoded_samples
            .checked_add(u64::try_from(FRAME_SAMPLES).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("Opus encoded sample count overflow"))?;
        let granule_position = self
            .encoded_samples
            .checked_mul(u64::from(GRANULE_RATE_MULTIPLIER))
            .ok_or_else(|| io::Error::other("Ogg granule position overflow"))?;

        if let Some(previous) = self.pending.replace(PendingPacket {
            data: output,
            granule_position,
        }) {
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Ogg writer is already closed"))?;
            writer.write_packet(
                previous.data,
                self.serial,
                PacketWriteEndInfo::NormalPacket,
                previous.granule_position,
            )?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn sync_output_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // Keep the platform-specific durability hook fallible.
fn sync_output_directory(_path: &Path) -> io::Result<()> {
    // Windows does not expose a stable std API for opening a directory with
    // FILE_FLAG_BACKUP_SEMANTICS and flushing its metadata. The file handle
    // itself is flushed by durably_commit; directory-entry durability remains
    // subject to the filesystem's guarantees.
    Ok(())
}

impl Drop for OggOpusRecorder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn opus_head(pre_skip: u16) -> [u8; 19] {
    let mut header = [0; 19];
    header[..8].copy_from_slice(b"OpusHead");
    header[8] = 1;
    header[9] = CHANNELS;
    header[10..12].copy_from_slice(&pre_skip.to_le_bytes());
    header[12..16].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    // Output gain is zero and mapping family 0 is mono/stereo, so the
    // zero-initialized tail is already correct.
    header
}

fn opus_tags() -> Vec<u8> {
    let mut tags = Vec::with_capacity(8 + 4 + VENDOR.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&4_u32.to_le_bytes());
    tags.extend_from_slice(VENDOR.as_bytes());
    tags.extend_from_slice(&0_u32.to_le_bytes());
    tags
}

fn codec_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

trait DurableSink {
    fn flush_sink(&mut self) -> io::Result<()>;
    fn sync_sink(&mut self) -> io::Result<()>;
}

impl DurableSink for BufWriter<File> {
    fn flush_sink(&mut self) -> io::Result<()> {
        self.flush()
    }

    fn sync_sink(&mut self) -> io::Result<()> {
        self.get_ref().sync_all()
    }
}

fn durably_commit(sink: &mut impl DurableSink) -> io::Result<()> {
    sink.flush_sink()?;
    sink.sync_sink()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use ogg::{Packet, PacketReader};
    use ropus::{Channels, DecodeMode, Decoder};

    use super::{DurableSink, GRANULE_RATE_MULTIPLIER, OggOpusRecorder, durably_commit};

    fn read_packets(path: &std::path::Path) -> Vec<Packet> {
        let mut reader = PacketReader::new(File::open(path).unwrap());
        let mut packets = Vec::new();
        while let Some(packet) = reader.read_packet().unwrap() {
            packets.push(packet);
        }
        packets
    }

    #[test]
    fn writes_headers_audio_and_exact_end_granule() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.ogg");
        let samples = vec![0.125; 777];
        let mut recorder = OggOpusRecorder::create(&path).unwrap();
        let pre_skip = recorder.pre_skip;

        recorder.push(&samples[..123]).unwrap();
        recorder.push(&samples[123..]).unwrap();
        recorder.finish().unwrap();

        let mut reader = PacketReader::new(File::open(path).unwrap());
        let head = reader.read_packet().unwrap().unwrap();
        let tags = reader.read_packet().unwrap().unwrap();
        assert!(head.first_in_stream());
        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(&tags.data[..8], b"OpusTags");

        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        let mut pcm_output = vec![0.0; 5_760];
        let mut last = reader.read_packet().unwrap().unwrap();
        assert_eq!(
            decoder
                .decode_float(&last.data, &mut pcm_output, DecodeMode::Normal)
                .unwrap(),
            960
        );
        while let Some(packet) = reader.read_packet().unwrap() {
            assert_eq!(
                decoder
                    .decode_float(&packet.data, &mut pcm_output, DecodeMode::Normal)
                    .unwrap(),
                960
            );
            last = packet;
        }
        assert!(last.last_in_stream());
        assert_eq!(
            last.absgp_page(),
            u64::from(pre_skip)
                + u64::try_from(samples.len()).unwrap() * u64::from(GRANULE_RATE_MULTIPLIER)
        );
    }

    #[test]
    fn empty_recording_is_a_valid_ended_stream() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.ogg");
        let mut recorder = OggOpusRecorder::create(&path).unwrap();
        recorder.finish().unwrap();

        let mut reader = PacketReader::new(File::open(path).unwrap());
        let _head = reader.read_packet().unwrap().unwrap();
        let _tags = reader.read_packet().unwrap().unwrap();
        let audio = reader.read_packet().unwrap().unwrap();
        assert!(audio.last_in_stream());
        assert!(reader.read_packet().unwrap().is_none());
    }

    #[test]
    fn refuses_to_replace_an_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.ogg");
        fs::write(&path, b"keep me").unwrap();

        let error = OggOpusRecorder::create(&path).err().unwrap();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), b"keep me");
    }

    #[cfg(unix)]
    #[test]
    fn creates_owner_only_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.ogg");

        let recorder = OggOpusRecorder::create(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        drop(recorder);

        assert_eq!(mode, 0o600);
    }

    #[test]
    fn finish_is_idempotent_and_push_after_finish_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("finished.ogg");
        let mut recorder = OggOpusRecorder::create(&path).unwrap();
        recorder.push(&[0.25; 17]).unwrap();

        recorder.finish().unwrap();
        recorder.finish().unwrap();
        assert!(recorder.push(&[0.5]).is_err());

        let packets = read_packets(&path);
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.last_in_stream())
                .count(),
            1
        );
    }

    #[test]
    fn exact_frame_boundary_and_partial_frame_both_end_cleanly() {
        for (name, sample_count) in [("boundary.ogg", 320), ("partial.ogg", 321)] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(name);
            let mut recorder = OggOpusRecorder::create(&path).unwrap();
            recorder.push(&vec![0.125; sample_count]).unwrap();
            let pre_skip = recorder.pre_skip;
            recorder.finish().unwrap();

            let packets = read_packets(&path);
            let last = packets.last().unwrap();
            assert!(last.last_in_stream());
            assert_eq!(
                last.absgp_page(),
                u64::from(pre_skip)
                    + u64::try_from(sample_count).unwrap() * u64::from(GRANULE_RATE_MULTIPLIER)
            );
        }
    }

    #[derive(Default)]
    struct FaultSink {
        flush_error: bool,
        sync_error: bool,
        flushes: usize,
        syncs: usize,
    }

    impl DurableSink for FaultSink {
        fn flush_sink(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.flush_error {
                Err(std::io::Error::other("flush fault"))
            } else {
                Ok(())
            }
        }

        fn sync_sink(&mut self) -> std::io::Result<()> {
            self.syncs += 1;
            if self.sync_error {
                Err(std::io::Error::other("sync fault"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn durability_reports_flush_and_sync_faults_in_order() {
        let mut flush_fault = FaultSink {
            flush_error: true,
            ..FaultSink::default()
        };
        assert_eq!(
            durably_commit(&mut flush_fault).unwrap_err().to_string(),
            "flush fault"
        );
        assert_eq!((flush_fault.flushes, flush_fault.syncs), (1, 0));

        let mut sync_fault = FaultSink {
            sync_error: true,
            ..FaultSink::default()
        };
        assert_eq!(
            durably_commit(&mut sync_fault).unwrap_err().to_string(),
            "sync fault"
        );
        assert_eq!((sync_fault.flushes, sync_fault.syncs), (1, 1));
    }
}
