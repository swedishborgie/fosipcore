//! FLV audio tag construction and stream state for AAC-in-FLV.
//!
//! The browser expects one **AAC sequence header tag** (the `AudioSpecificConfig`,
//! config type 0) per client, followed by repeating **AAC raw tags**
//! (config type 1).
//!
//! `AudioTag` header byte layout (FLV spec):
//!
//! ```text
//! SoundFormat(4) | SamplingRate(2) | SoundSize(1) | SoundType(1)
//! ```
//!
//! For AAC, `SoundFormat = 10`. The 2-bit `SamplingRate` field is a
//! legacy hint — `flv.js` reads the real rate from the
//! `AudioSpecificConfig` — so we set it to `3` (44.1 kHz), the common
//! convention for AAC-in-FLV. `SoundType = 1` (mono).

/// Build a 13-byte FLV header for an **audio-only** stream (flags
/// `0x04` = hasAudio, no video), followed by a zero `PreviousTagSize`.
///
/// The video pipeline's `flv_muxer::flv_header()` uses flags `0x01`
/// (video-only); flv.js expects the audio bit set on `stream2.flv`.
#[must_use]
pub fn flv_audio_header() -> Vec<u8> {
    vec![
        b'F', b'L', b'V', 1, 0x04, // header, version 1, flags: audio only
        0, 0, 0, 9, // DataOffset = 9
        0, 0, 0, 0, // PreviousTagSize = 0
    ]
}

/// Build an AAC **sequence header** FLV tag (config type 0).
///
/// Payload: `0x00` (AAC sequence header) + `AudioSpecificConfig`.
#[must_use]
pub fn aac_sequence_header_tag(asc: &[u8], timestamp_ms: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + asc.len());
    payload.push(0x00); // AAC sequence header
    payload.extend_from_slice(asc);
    build_audio_tag(&payload, timestamp_ms)
}

/// Build an AAC **raw** FLV tag (config type 1).
///
/// Payload: `0x01` (AAC raw) + the raw AAC access unit.
#[must_use]
pub fn aac_raw_tag(aac_data: &[u8], timestamp_ms: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + aac_data.len());
    payload.push(0x01); // AAC raw
    payload.extend_from_slice(aac_data);
    build_audio_tag(&payload, timestamp_ms)
}

/// `AudioTag` header byte: `SoundFormat`=10 (AAC), `SamplingRate`=3
/// (legacy 44.1k hint), `SoundSize`=0 (16-bit), `SoundType`=1 (mono).
const AUDIO_TAG_HEADER_AAC_MONO: u8 = (10u8 << 4) | (3u8 << 2) | 1u8;

/// Build a complete FLV audio tag (header + payload + `PreviousTagSize`).
///
/// The tag body is the 1-byte `AudioTag` header (`SoundFormat` etc.)
/// followed by the AAC payload; both are counted in `DataSize` and
/// `PreviousTagSize`.
fn build_audio_tag(payload: &[u8], timestamp_ms: u32) -> Vec<u8> {
    let data_size = 1 + payload.len();
    let data_size_u32 = u32::try_from(data_size).unwrap_or(u32::MAX);
    let mut tag = Vec::with_capacity(11 + data_size + 4);
    tag.extend_from_slice(&build_tag_header(0x08, data_size_u32, timestamp_ms));
    tag.push(AUDIO_TAG_HEADER_AAC_MONO);
    tag.extend_from_slice(payload);
    let prev_size = 11 + data_size_u32;
    tag.extend_from_slice(&prev_size.to_be_bytes());
    tag
}

/// Build an 11-byte FLV tag header (same layout as the video muxer).
fn build_tag_header(tag_type: u8, data_size: u32, timestamp: u32) -> [u8; 11] {
    let mut hdr = [0u8; 11];
    hdr[0] = tag_type;
    hdr[1] = ((data_size >> 16) & 0xFF) as u8;
    hdr[2] = ((data_size >> 8) & 0xFF) as u8;
    hdr[3] = (data_size & 0xFF) as u8;
    hdr[4] = ((timestamp >> 16) & 0xFF) as u8;
    hdr[5] = ((timestamp >> 8) & 0xFF) as u8;
    hdr[6] = (timestamp & 0xFF) as u8;
    hdr[7] = ((timestamp >> 24) & 0xFF) as u8;
    hdr
}

/// Per-stream audio muxer state: holds the cached AAC sequence header
/// tag (so late-joining clients get it) and the RTP→FLV timestamp base.
pub struct AudioFlvMuxer {
    /// Cached AAC sequence header tag (built once, reused per client).
    sequence_header_tag: Vec<u8>,
    /// RTP timestamp (8 kHz clock) of the first encoded sample — the
    /// base for FLV millisecond timestamps.
    rtp_base: Option<u64>,
}

impl AudioFlvMuxer {
    /// Create a new audio muxer. `asc` is the encoder's
    /// `AudioSpecificConfig`; the sequence header tag is built eagerly.
    #[must_use]
    pub fn new(asc: &[u8]) -> Self {
        Self {
            sequence_header_tag: aac_sequence_header_tag(asc, 0),
            rtp_base: None,
        }
    }

    /// The cached AAC sequence header tag (for late-joining clients).
    #[must_use]
    pub fn sequence_header_tag(&self) -> &[u8] {
        &self.sequence_header_tag
    }
}

/// Convert an access unit's sample-domain PTS to an FLV timestamp in
/// milliseconds, anchored so the first unit is ~0.
///
/// `pts_samples` is relative to stream start; `sample_rate` is the
/// encode rate (8 kHz). FLV timestamps are 32-bit, so the result
/// saturates at `u32::MAX`.
#[must_use]
pub fn pts_to_ms(pts_samples: u64, sample_rate: u32) -> u32 {
    let ms = pts_samples * 1_000 / u64::from(sample_rate);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

impl AudioFlvMuxer {
    /// Mark the RTP base as set (called with the first audio RTP
    /// timestamp so A/V can share a time base). Retained for clarity —
    /// PTS passed to [`pts_to_ms`] is already relative.
    pub fn set_rtp_base(&mut self, rtp_ts: u64) {
        if self.rtp_base.is_none() {
            self.rtp_base = Some(rtp_ts);
            tracing::debug!("FLV audio: RTP base = {rtp_ts} (8 kHz clock)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real ASC for 8 kHz mono MPEG-4 AAC-LC (freqIdx 11, chan 1).
    const ASC_8K_MONO: [u8; 2] = [0x15, 0x88];

    #[test]
    fn sequence_header_tag_layout() {
        let asc = ASC_8K_MONO;
        let tag = aac_sequence_header_tag(&asc, 0);

        // 11-byte tag header + 1-byte AudioTag header + (1 + 2) payload
        // + 4-byte PreviousTagSize.
        assert_eq!(tag.len(), 11 + 1 + 3 + 4);
        assert_eq!(tag[0], 0x08); // audio tag
                                  // DataSize = 1 (audio header) + 3 (payload) = 4.
        assert_eq!(u32::from_be_bytes([0, tag[1], tag[2], tag[3]]), 4);
        assert_eq!(tag[11], AUDIO_TAG_HEADER_AAC_MONO);
        assert_eq!(tag[12], 0x00); // config type: sequence header
        assert_eq!(&tag[13..15], &asc);
        // PreviousTagSize = 11 + 4.
        assert_eq!(u32::from_be_bytes([tag[15], tag[16], tag[17], tag[18]]), 15);
    }

    #[test]
    fn raw_tag_layout() {
        let tag = aac_raw_tag(&[0xAB, 0xCD], 128);
        // 11 + 1 (audio header) + 3 (0x01 + 2 data) + 4.
        assert_eq!(tag.len(), 11 + 1 + 3 + 4);
        assert_eq!(tag[0], 0x08);
        // Timestamp 128 ms in the tag header (extension byte at [7]).
        assert_eq!(u32::from_be_bytes([tag[7], tag[4], tag[5], tag[6]]), 128);
        assert_eq!(tag[11], AUDIO_TAG_HEADER_AAC_MONO);
        assert_eq!(tag[12], 0x01); // config type: AAC raw
        assert_eq!(&tag[13..15], &[0xAB, 0xCD]);
        // PreviousTagSize = 11 + 4.
        assert_eq!(u32::from_be_bytes([tag[15], tag[16], tag[17], tag[18]]), 15);
    }

    #[test]
    fn audio_header_layout() {
        let hdr = flv_audio_header();
        assert_eq!(hdr.len(), 13);
        assert_eq!(&hdr[0..3], b"FLV");
        assert_eq!(hdr[3], 1, "version");
        assert_eq!(hdr[4], 0x04, "audio-only flags");
        assert_eq!(u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]), 9);
        assert_eq!(u32::from_be_bytes([hdr[9], hdr[10], hdr[11], hdr[12]]), 0);
    }

    #[test]
    fn pts_conversion_8khz() {
        // One 1024-sample frame at 8 kHz = 128 ms.
        assert_eq!(pts_to_ms(1024, 8000), 128);
        assert_eq!(pts_to_ms(0, 8000), 0);
        assert_eq!(pts_to_ms(16_000, 8000), 2000);
    }
}
