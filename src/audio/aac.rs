//! AAC-LC encoder wrapper over `oxideav-aac` (pure Rust, no C/FFI).
//!
//! Design decisions:
//! - **No resampling.** The camera sends G.711 at 8 kHz; we encode at
//!   8 kHz directly, mirroring the source rate. `flv.js`/`WebAudio`
//!   upsamples to the output device rate transparently.
//! - **32 kbps target**, mono — matching the bitrate of the camera's
//!   original AAC audio stream.
//! - The encoder emits **ADTS-framed** access units; we strip the ADTS
//!   header to get the raw `raw_data_block` that FLV audio tags carry.
//! - The **`AudioSpecificConfig`** (the 2-byte AAC sequence header) is
//!   derived from the encoder's actual settings via
//!   `asc_writer::aac_lc_asc`.

use oxideav_aac::adts::AdtsHeader;
use oxideav_aac::asc_writer;
use oxideav_aac::encoder::{EncoderConfig, StreamEncoder};

/// The camera's G.711 audio rate — we mirror it, no resampling.
pub const AUDIO_SAMPLE_RATE: u32 = 8_000;
/// Bitrate of the camera's original AAC audio stream.
pub const AUDIO_BITRATE_BPS: u32 = 32_000;
/// Camera mic is single-channel (verified on a live unit).
pub const AUDIO_CHANNELS: u8 = 1;

/// Streaming AAC-LC encoder that buffers PCM and emits raw access units.
///
/// `push_pcm` accepts arbitrary sample counts (RTP packet sized); an
/// access unit is emitted for every full 1024-sample frame
/// (`oxideav_aac::encoder::FRAME_LEN`), which is 128 ms at 8 kHz.
pub struct AacEncoder {
    encoder: StreamEncoder,
    /// Samples buffered since the last emitted access unit.
    pending: Vec<i16>,
    /// Total samples accepted (including pending) — sample-domain PTS.
    samples_total: u64,
    /// `AudioSpecificConfig` (AAC sequence header) for this stream.
    asc: Vec<u8>,
    sample_rate: u32,
    channels: u8,
}

/// One emitted AAC access unit.
#[derive(Debug, Clone)]
pub struct AacAccessUnit {
    /// Raw `raw_data_block` bytes (no ADTS framing).
    pub data: Vec<u8>,
    /// Presentation timestamp in samples (first sample of the unit).
    pub pts: u64,
    /// Duration in samples (always one AAC frame = 1024).
    pub duration: u64,
}

impl AacEncoder {
    /// Create a new encoder for the given stream parameters.
    ///
    /// # Errors
    ///
    /// `Err` if the encoder rejects the configuration (unsupported
    /// sample rate or channel count).
    pub fn new(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Result<Self, anyhow::Error> {
        let config = EncoderConfig {
            sample_rate,
            channels,
            bitrate: bitrate_bps,
        };
        let encoder = StreamEncoder::new(config)
            .map_err(|e| anyhow::anyhow!("AAC encoder init failed: {e}"))?;
        // Derive the ASC from the actual encode settings — never hard-code.
        let asc = asc_writer::aac_lc_asc(sample_rate, channels);
        tracing::info!(
            "AAC encoder: {sample_rate} Hz, {channels} ch, {bitrate_bps} bps, ASC={asc:02x?}"
        );
        Ok(Self {
            encoder,
            pending: Vec::new(),
            samples_total: 0,
            asc,
            sample_rate,
            channels,
        })
    }

    /// The AAC sequence header (`AudioSpecificConfig`) bytes.
    #[must_use]
    pub fn asc(&self) -> &[u8] {
        &self.asc
    }

    /// Sample rate the encoder is configured for.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Channel count the encoder is configured for.
    #[must_use]
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Buffer interleaved `i16` PCM samples and emit any completed
    /// access units.
    ///
    /// Returns access units in PTS order; empty if no full 1024-sample
    /// frame has accumulated yet.
    ///
    /// # Errors
    ///
    /// `Err` if the underlying encoder fails (should not happen for
    /// valid input).
    pub fn push_pcm(&mut self, pcm: &[i16]) -> Result<Vec<AacAccessUnit>, anyhow::Error> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }

        let frame_len = oxideav_aac::encoder::FRAME_LEN * usize::from(self.channels);
        self.pending.extend_from_slice(pcm);
        // usize → u64: widening on 32-bit, identity on 64-bit; never truncates.
        self.samples_total += pcm.len() as u64 / u64::from(self.channels);

        let mut units = Vec::new();
        while self.pending.len() >= frame_len {
            let unit_pts =
                self.samples_total - self.pending.len() as u64 / u64::from(self.channels);
            // Drain the frame out so the encoder borrow doesn't conflict.
            let frame: Vec<i16> = self.pending.drain(..frame_len).collect();

            let adts_frame = self
                .encoder
                .encode_frame(&frame)
                .map_err(|e| anyhow::anyhow!("AAC encode_frame failed: {e}"))?;

            // Strip the ADTS header → raw_data_block for the FLV tag.
            let (_, payload_offset) = AdtsHeader::parse(&adts_frame)
                .map_err(|e| anyhow::anyhow!("AAC: malformed ADTS frame: {e}"))?;
            let raw = adts_frame[payload_offset..].to_vec();
            if raw.is_empty() {
                return Err(anyhow::anyhow!("AAC: empty access unit"));
            }

            units.push(AacAccessUnit {
                data: raw,
                pts: unit_pts,
                duration: frame_len as u64,
            });
        }
        Ok(units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a 440 Hz sine and check the encoder produces valid output.
    // Test-only casts: i < 16000 (exact in f32), amplitude 10_000 < i16::MAX.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    #[test]
    fn encodes_sine_wave() {
        let mut enc = AacEncoder::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, AUDIO_BITRATE_BPS)
            .expect("encoder init");

        // 2 seconds of 440 Hz sine at 8 kHz (16000 samples → 15 frames).
        // Amplitude 10_000 < i16::MAX, so the cast below is value-safe.
        let samples: Vec<i16> = (0..16_000)
            .map(|i| {
                let t = std::f32::consts::TAU * 440.0 * i as f32 / 8000.0;
                (10_000.0 * t.sin()) as i16
            })
            .collect();

        let mut units = Vec::new();
        // Feed in RTP-sized chunks (160 samples = 20 ms).
        for chunk in samples.chunks(160) {
            units.extend(enc.push_pcm(chunk).unwrap());
        }

        // 16000 / 1024 = 15 complete frames.
        assert_eq!(units.len(), 15, "expected 15 access units");
        // PTS must advance by exactly one frame per unit.
        for (i, u) in units.iter().enumerate() {
            assert_eq!(u.pts, i as u64 * 1024, "unit {i} pts");
            assert_eq!(u.duration, 1024);
            assert!(!u.data.is_empty());
        }
        // ASC for 8 kHz mono MPEG-4 AAC-LC: freqIdx 11 (0x15 0x88), chan 1.
        assert_eq!(enc.asc(), &[0x15, 0x88]);
    }

    #[test]
    fn partial_frame_buffered() {
        let mut enc = AacEncoder::new(8000, 1, 32_000).unwrap();
        // Less than one frame → no output, no error.
        let units = enc.push_pcm(&[100i16; 100]).unwrap();
        assert!(units.is_empty());
        // Top up past the 1024 boundary → exactly one unit.
        let units = enc.push_pcm(&[100i16; 1_000]).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].pts, 0);
    }
}
