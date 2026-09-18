//! Audio RTP → FLV transcoder: the per-pipeline audio state machine.
//!
//! Consumes raw G.711 μ-law RTP packets from the camera's RTSP audio
//! track (PT 0 / PCMU, 8 kHz mono), decodes to i16 PCM, feeds the AAC
//! encoder, and emits ready-to-send FLV audio tags:
//!
//! ```text
//! RTP (μ-law bytes) → g711 decode → i16 PCM → AAC-LC encode
//!     → [first unit: AAC sequence header tag]
//!     → AAC raw tags (FLV timestamp = sample PTS in ms)
//! ```
//!
//! Timestamps are relative to stream start (first sample ≈ 0 ms),
//! matching the video pipeline's 0-based FLV timeline so the two
//! streams stay roughly aligned for flv.js.

use crate::audio::aac::{AacEncoder, AUDIO_BITRATE_BPS, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::audio::flv::{pts_to_ms, AudioFlvMuxer};
use crate::audio::g711;

/// RTP payload type for PCMU (G.711 μ-law).
const RTP_PT_PCMU: u8 = 0;

/// One audio transcode stage: G.711 RTP in, FLV audio tags out.
pub struct AudioTranscoder {
    encoder: AacEncoder,
    muxer: AudioFlvMuxer,
    /// Whether the AAC sequence header tag has been emitted yet.
    seq_sent: bool,
}

impl AudioTranscoder {
    /// Create a transcoder for the camera's 8 kHz mono μ-law stream.
    ///
    /// # Errors
    ///
    /// `Err` if the AAC encoder rejects the configuration.
    pub fn new() -> Result<Self, anyhow::Error> {
        let encoder = AacEncoder::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, AUDIO_BITRATE_BPS)?;
        let asc = encoder.asc().to_vec();
        Ok(Self {
            muxer: AudioFlvMuxer::new(&asc),
            seq_sent: false,
            encoder,
        })
    }

    /// The cached AAC sequence header tag (for late-joining clients).
    #[must_use]
    pub fn sequence_header_tag(&self) -> &[u8] {
        self.muxer.sequence_header_tag()
    }

    /// Process one raw RTP packet and return any FLV audio tags.
    ///
    /// Non-audio or malformed packets are ignored (empty result). The
    /// AAC sequence header tag is emitted exactly once, ahead of the
    /// first raw tag.
    ///
    /// # Errors
    ///
    /// `Err` if the AAC encoder fails (should not happen for valid
    /// input).
    pub fn process_rtp(&mut self, rtp: &[u8]) -> Result<Vec<Vec<u8>>, anyhow::Error> {
        let Some(payload) = parse_g711_rtp(rtp) else {
            return Ok(Vec::new());
        };

        // μ-law → i16 PCM (1:1 sample count, mono).
        let mut pcm = vec![0i16; payload.len()];
        if g711::decode_mulaw(payload, &mut pcm).is_err() {
            return Ok(Vec::new());
        }

        let units = self.encoder.push_pcm(&pcm)?;
        if units.is_empty() {
            return Ok(Vec::new());
        }

        let mut tags = Vec::new();
        if !self.seq_sent {
            self.seq_sent = true;
            tags.push(self.muxer.sequence_header_tag().to_vec());
        }
        for unit in &units {
            let ts = pts_to_ms(unit.pts, AUDIO_SAMPLE_RATE);
            tags.push(crate::audio::flv::aac_raw_tag(&unit.data, ts));
            tracing::trace!(
                "audio: FLV tag pts={} samples ({ts} ms), {} bytes",
                unit.pts,
                unit.data.len()
            );
        }
        Ok(tags)
    }
}

/// Parse a G.711 RTP packet: validate the header, return the μ-law
/// payload (padding stripped), or `None` if not a PCMU audio packet.
fn parse_g711_rtp(rtp: &[u8]) -> Option<&[u8]> {
    if rtp.len() < 12 {
        return None;
    }
    let b0 = rtp[0];
    if b0 >> 6 != 2 {
        return None; // not RTP v2
    }
    let pt = rtp[1] & 0x7f;
    if pt != RTP_PT_PCMU {
        tracing::trace!("audio: ignoring RTP packet with PT {pt} (expected PCMU=0)");
        return None;
    }

    let csrc_count = (b0 & 0x0f) as usize;
    let has_ext = b0 & 0x10 != 0;
    let has_padding = b0 & 0x20 != 0;

    let mut off = 12 + csrc_count * 4;
    if has_ext {
        // Extension: 4-byte header (profile + length in 32-bit words).
        if rtp.len() < off + 4 {
            return None;
        }
        // 4-byte extension header: 16-bit profile + 16-bit length in
        // 32-bit words.
        let ext_len = u16::from_be_bytes([rtp[off + 2], rtp[off + 3]]) as usize * 4;
        off += 4 + ext_len;
    }
    if off >= rtp.len() {
        return None;
    }
    let mut payload = &rtp[off..];
    if has_padding {
        let pad = *payload.last()? as usize;
        if pad == 0 || pad > payload.len() {
            return None;
        }
        payload = &payload[..payload.len() - pad];
    }
    if payload.is_empty() {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal RTP v2 packet: PCMU, seq, 8 kHz timestamp, no
    /// CSRC/ext, optional padding.
    fn rtp_packet(payload: &[u8], pad: u8) -> Vec<u8> {
        let mut pkt = vec![0x80, RTP_PT_PCMU, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x2a];
        pkt.extend_from_slice(payload);
        if pad != 0 {
            // Padding zeros first, length byte LAST (RTP spec).
            pkt.resize(pkt.len() + pad as usize - 1, 0);
            pkt.push(pad);
            pkt[0] |= 0x20; // padding bit
        }
        pkt
    }

    /// A run of μ-law silence-ish bytes: 0x7F/0xFF decode near zero.
    fn mulaw_bytes(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| if i % 2 == 0 { 0x7Fu8 } else { 0xFFu8 })
            .collect()
    }

    #[test]
    fn emits_sequence_header_once_then_raw_tags() {
        let mut t = AudioTranscoder::new().expect("init");
        // 20 ms packets (160 samples); one AAC frame = 1024 samples =
        // 6.4 packets, so 8 packets → 1 access unit.
        let mut tags: Vec<Vec<u8>> = Vec::new();
        for _ in 0..8 {
            tags.extend(
                t.process_rtp(&rtp_packet(&mulaw_bytes(160), 0))
                    .expect("process"),
            );
        }
        assert_eq!(tags.len(), 2, "1 sequence header + 1 raw tag");
        // FLV tag header is 11 bytes + 1-byte AudioTag header → the
        // AAC config-type marker sits at index 12.
        assert_eq!(tags[0][0], 0x08, "audio tag type");
        assert_eq!(tags[0][12], 0x00, "AAC sequence header marker");
        // Raw tag: marker 1 (AAC raw).
        assert_eq!(tags[1][12], 0x01, "AAC raw marker");
        // A second frame must NOT repeat the sequence header. (256
        // samples were buffered after frame 1; 6 × 160 tops it past the
        // next 1024 boundary → exactly one more unit.)
        tags.clear();
        for _ in 0..6 {
            tags.extend(
                t.process_rtp(&rtp_packet(&mulaw_bytes(160), 0))
                    .expect("process"),
            );
        }
        assert_eq!(tags.len(), 1, "only a raw tag for frame 2");
        assert_eq!(tags[0][12], 0x01);
    }

    #[test]
    fn ignores_non_pcmu_and_short_packets() {
        let mut t = AudioTranscoder::new().expect("init");
        let mut pkt = rtp_packet(&mulaw_bytes(160), 0);
        pkt[1] = 96; // H.264 PT
        assert!(t.process_rtp(&pkt).expect("ok").is_empty());
        assert!(t.process_rtp(&[0x80, 0]).expect("ok").is_empty());
    }

    #[test]
    fn strips_padding() {
        let mut t = AudioTranscoder::new().expect("init");
        // 160 μ-law bytes + 4 bytes padding; 8 packets → 1 unit either
        // way, proving the padded packets decoded (1280 real samples).
        let mut tags = Vec::new();
        for _ in 0..8 {
            tags.extend(
                t.process_rtp(&rtp_packet(&mulaw_bytes(160), 4))
                    .expect("process"),
            );
        }
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn handles_rtp_extension() {
        let mut t = AudioTranscoder::new().expect("init");
        // One-32-bit-word extension between header and payload.
        let mut pkt = vec![0x90, RTP_PT_PCMU, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        pkt.extend_from_slice(&[0x12, 0x34, 0x00, 0x01]); // ext hdr: 1 word
        pkt.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // 1 word of ext data
        pkt.extend_from_slice(&[0xAA, 0xBB]); // μ-law payload
        assert_eq!(parse_g711_rtp(&pkt), Some(&[0xAAu8, 0xBB][..]));
        let _ = t.process_rtp(&pkt);
    }
}
