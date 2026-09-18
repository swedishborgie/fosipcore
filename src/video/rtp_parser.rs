/// RTP packet parser: extracts H.264 NAL units from RTP payloads.
///
/// ## RTP Header (12 bytes)
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |V=2|P|X|  CC   |M|   PT      |       sequence number         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                           timestamp                           |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |           synchronization source (SSRC) identifier            |
/// +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
/// ```
///
/// ## NAL Unit Types (H.264)
///
/// The NAL type is the lower 5 bits of the first byte after the RTP payload header.
///
/// | Type  | Name              | Meaning                              |
/// |-------|-------------------|---------------------------------------|
/// | 1     | Coded slice       | Non-IDR picture (P-frame)             |
/// | 5     | Coded slice IDR   | Keyframe (I-frame)                    |
/// | 7     | SPS               | Sequence parameter set                |
/// | 8     | PPS               | Picture parameter set                 |
/// | 24    | STAP-A            | Single-time aggregation packet        |
/// | 28    | FU-A              | Fragmentation unit                    |
///
/// A parsed NAL unit ready for FLV muxing.
#[derive(Debug, Clone)]
pub struct NalUnit {
    /// Raw NAL unit data (without start code).
    pub data: Vec<u8>,
    /// NAL unit type (lower 5 bits of the header byte).
    pub nal_type: u8,
    /// Whether this is a keyframe (type 5 = IDR).
    pub is_keyframe: bool,
    /// RTP timestamp (90kHz clock).
    pub timestamp: u32,
}

/// NAL unit type constants.
const NAL_TYPE_SLICE: u8 = 1;
const NAL_TYPE_IDR: u8 = 5;
const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;
const NAL_TYPE_STAP_A: u8 = 24;
const NAL_TYPE_FU_A: u8 = 28;

/// RTP parser that handles FU-A fragmentation reassembly.
pub struct RtpParser {
    /// Buffer for reassembling fragmented NAL units across RTP packets.
    ///
    /// Hard-capped at [`MAX_FU_BUFFER_BYTES`]: a rogue/MITMed camera that
    /// streams continuation fragments without an end bit must not grow this
    /// without bound (each interleaved RTP packet can carry up to 64 KiB).
    fu_buffer: Vec<u8>,
    /// Whether we're currently reassembling a fragmented NAL.
    fu_active: bool,
}

/// Cap on in-flight FU-A reassembly (bytes). A legitimate 2560×1440 H.264
/// frame is at most a couple hundred KB; 1 MiB is generous headroom.
const MAX_FU_BUFFER_BYTES: usize = 1024 * 1024;

/// Append `data` to the FU-A reassembly buffer, enforcing the size cap.
///
/// Returns `false` (and leaves the buffer unchanged) when the append would
/// exceed [`MAX_FU_BUFFER_BYTES`].
fn fu_buffer_append(buf: &mut Vec<u8>, data: &[u8]) -> bool {
    if buf.len() + data.len() > MAX_FU_BUFFER_BYTES {
        return false;
    }
    buf.extend_from_slice(data);
    true
}

impl RtpParser {
    /// Create a new RTP parser with no active reassembly.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::new(),
            fu_active: false,
        }
    }

    /// Parse an RTP payload and extract any complete NAL units.
    ///
    /// Returns `Some(NalUnit)` for complete NAL units, or `None` when
    /// a fragmented unit is still being assembled across RTP packets.
    ///
    /// The RTP payload starts with the RTP header. We skip the 12-byte
    /// header to reach the RTP payload data. For H.264, the payload
    /// format is described in RFC 6184.
    ///
    /// # Arguments
    /// * `payload` - Raw bytes from the RTP packet (starting with `$` interleaved frame data: the RTP header + NAL payload)
    /// * `timestamp` - RTP timestamp from the RTP header (90kHz for H.264)
    pub fn parse(&mut self, payload: &[u8], timestamp: u32) -> Option<NalUnit> {
        if payload.len() < 13 {
            // Need at least 12-byte RTP header + 1 byte NAL header
            tracing::warn!("RTP: payload too short ({} bytes)", payload.len());
            return None;
        }

        // Skip 12-byte RTP header (we don't need sequence number/SSRC here)
        // The timestamp is passed in separately.
        let rtp_payload = &payload[12..];

        if rtp_payload.is_empty() {
            return None;
        }

        // The first byte is the NAL unit header:
        //   F (1 bit) | NRI (2 bits) | Type (5 bits)
        let nal_header = rtp_payload[0];
        let nal_type = nal_header & 0x1F;

        match nal_type {
            // Single NAL unit packet (types 1-23)
            NAL_TYPE_SLICE | NAL_TYPE_IDR | NAL_TYPE_SPS | NAL_TYPE_PPS => {
                // The whole RTP payload is one NAL unit.
                // Convert to AVCC format: 4-byte big-endian length prefix
                // instead of annex-B start codes.
                let avcc = to_avcc_format(rtp_payload);
                let is_keyframe = nal_type == NAL_TYPE_IDR;

                tracing::trace!(
                    "RTP: single NAL type={}, {} bytes, keyframe={is_keyframe}",
                    nal_type,
                    avcc.len()
                );

                Some(NalUnit {
                    data: avcc,
                    nal_type,
                    is_keyframe,
                    timestamp,
                })
            }

            // FU-A fragmentation unit
            NAL_TYPE_FU_A => self.parse_fu_a(rtp_payload, timestamp),

            // STAP-A aggregation packet (multiple NALs in one)
            NAL_TYPE_STAP_A => self.parse_stap_a(rtp_payload, timestamp),

            // Other types — pass through as single NAL
            _ => {
                let avcc = to_avcc_format(rtp_payload);
                Some(NalUnit {
                    data: avcc,
                    nal_type,
                    is_keyframe: false,
                    timestamp,
                })
            }
        }
    }

    /// Parse a FU-A fragmentation unit.
    ///
    /// FU-A header (2 bytes):
    ///   Byte 0: F (1 bit) | NRI (2 bits) | 28 (5 bits)  — NAL header with type=28
    ///   Byte 1: S (1 bit) | E (1 bit) | R (1 bit) | Type (5 bits)
    ///
    ///   S=1: Start of fragmented NAL → begin reassembly
    ///   E=1: End of fragmented NAL → complete and emit
    ///   S=0, E=0: Continuation fragment
    fn parse_fu_a(&mut self, rtp_payload: &[u8], timestamp: u32) -> Option<NalUnit> {
        if rtp_payload.len() < 3 {
            tracing::warn!("RTP: FU-A payload too short ({} bytes)", rtp_payload.len());
            return None;
        }

        // FU header: first byte of RTP payload (already NAL header with type=28)
        let fu_header = rtp_payload[0];

        // FU indicator: second byte
        let fu_indicator = rtp_payload[1];
        let is_start = (fu_indicator & 0x80) != 0; // S bit
        let is_end = (fu_indicator & 0x40) != 0; // E bit
        let fu_type = fu_indicator & 0x1F;

        // FU data follows the 2-byte header
        let fu_data = &rtp_payload[2..];

        if is_start {
            // Begin new fragmentation unit
            // Reconstruct the NAL header: F + NRI from fu_header, type from fu_indicator
            let reconstructed_nal_header = (fu_header & 0xE0) | fu_type;
            self.fu_buffer.clear();
            self.fu_buffer.push(reconstructed_nal_header);
            if !fu_buffer_append(&mut self.fu_buffer, fu_data) {
                tracing::warn!("RTP: FU-A start fragment exceeds cap — dropping reassembly");
                self.fu_buffer.clear();
                self.fu_active = false;
                return None;
            }
            self.fu_active = true;

            tracing::trace!(
                "RTP: FU-A start, type={fu_type}, {} fragment bytes",
                fu_data.len()
            );
            None
        } else if is_end {
            // Complete fragmentation unit
            if !fu_buffer_append(&mut self.fu_buffer, fu_data) {
                tracing::warn!("RTP: FU-A unit exceeds cap — dropping reassembly");
                self.fu_buffer.clear();
                self.fu_active = false;
                return None;
            }
            self.fu_active = false;

            let nal_type = self.fu_buffer.first().copied().unwrap_or(0) & 0x1F;
            let is_keyframe = nal_type == NAL_TYPE_IDR;

            let avcc = to_avcc_format(&self.fu_buffer);

            tracing::trace!(
                "RTP: FU-A end, type={nal_type}, total {} bytes, keyframe={is_keyframe}",
                avcc.len()
            );

            let result = Some(NalUnit {
                data: avcc,
                nal_type,
                is_keyframe,
                timestamp,
            });

            self.fu_buffer.clear();
            result
        } else {
            // Continuation fragment
            if !self.fu_active {
                tracing::warn!("RTP: FU-A continuation without start — discarding");
                return None;
            }
            if !fu_buffer_append(&mut self.fu_buffer, fu_data) {
                // Hostile stream: endless fragments, no end bit. Reset so
                // the buffer stays bounded and parsing recovers on the next
                // start bit.
                tracing::warn!("RTP: FU-A reassembly exceeds {MAX_FU_BUFFER_BYTES} bytes — resetting");
                self.fu_buffer.clear();
                self.fu_active = false;
                return None;
            }

            tracing::trace!(
                "RTP: FU-A continuation, {} bytes in buffer",
                self.fu_buffer.len()
            );
            None
        }
    }

    /// Parse a STAP-A aggregation packet.
    ///
    /// STAP-A packs multiple NAL units into a single RTP packet:
    ///   NAL header (type=24) | NAL size (2 bytes BE) | NAL data | NAL size | NAL data | ...
    ///
    /// Returns only the LAST complete NAL in the aggregation (typically
    /// the picture slice data). The SPS/PPS are usually the first NALs
    /// in a STAP-A — those are processed separately.
    fn parse_stap_a(&mut self, rtp_payload: &[u8], timestamp: u32) -> Option<NalUnit> {
        // Skip the NAL header byte (type=24)
        let rest = &rtp_payload[1..];
        let mut offset = 0;
        let mut last_nal: Option<NalUnit> = None;

        while offset + 2 <= rest.len() {
            let size = u16::from_be_bytes([rest[offset], rest[offset + 1]]) as usize;
            offset += 2;

            if offset + size > rest.len() {
                tracing::warn!(
                    "RTP: STAP-A truncated at offset {offset}, size {size}, remaining {}",
                    rest.len()
                );
                break;
            }

            let nal_data = &rest[offset..offset + size];

            // The first byte of each NAL in STAP-A is the NAL header
            let nal_type = if nal_data.is_empty() {
                0
            } else {
                nal_data[0] & 0x1F
            };

            let is_keyframe = nal_type == NAL_TYPE_IDR;
            let avcc = to_avcc_format(nal_data);

            tracing::trace!("RTP: STAP-A NAL type={nal_type}, {size} bytes");

            last_nal = Some(NalUnit {
                data: avcc,
                nal_type,
                is_keyframe,
                timestamp,
            });

            offset += size;
        }

        last_nal
    }
}

impl Default for RtpParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert an annex-B format NAL unit to AVCC (MP4) format.
///
/// Annex-B uses start codes (0x00000001 or 0x000001).
/// AVCC uses a 4-byte big-endian length prefix.
fn to_avcc_format(nal_data: &[u8]) -> Vec<u8> {
    // Strip any annex-B start code prefix
    let nal = strip_start_code(nal_data);

    // Prepend 4-byte BE length
    let mut avcc = Vec::with_capacity(nal.len() + 4);
    let len = nal.len() as u32;
    avcc.extend_from_slice(&len.to_be_bytes());
    avcc.extend_from_slice(nal);
    avcc
}

/// Strip leading annex-B start codes:
/// - 4-byte: 0x00 0x00 0x00 0x01
/// - 3-byte: 0x00 0x00 0x01
fn strip_start_code(data: &[u8]) -> &[u8] {
    if data.len() >= 4 && data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x00 && data[3] == 0x01 {
        &data[4..]
    } else if data.len() >= 3 && data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x01 {
        &data[3..]
    } else {
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal RTP header (12 bytes).
    fn rtp_header(timestamp: u32) -> Vec<u8> {
        let mut hdr = vec![0u8; 12];
        // Version=2, no padding, no extension, CC=0
        hdr[0] = 0x80;
        // Marker=0, payload type=96
        hdr[1] = 96;
        // Sequence number (arbitrary)
        hdr[2..4].copy_from_slice(&1u16.to_be_bytes());
        // Timestamp
        hdr[4..8].copy_from_slice(&timestamp.to_be_bytes());
        // SSRC (arbitrary)
        hdr[8..12].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        hdr
    }

    #[test]
    fn test_parse_single_idr_nal() {
        let mut parser = RtpParser::new();
        let ts = 90_000;
        let mut payload = rtp_header(ts);

        // Single NAL: IDR slice (type 5)
        // NAL header: forbidden=0, NRI=2, type=5 → 0x25
        payload.push(0x25);
        payload.extend_from_slice(b"fake_idr_data");

        let nal = parser.parse(&payload, ts).expect("should parse single NAL");
        assert_eq!(nal.nal_type, 5);
        assert!(nal.is_keyframe);
        assert_eq!(nal.timestamp, ts);

        // AVCC format: 4-byte len prefix + data
        let expected_len = (payload.len() - 12) as u32;
        assert_eq!(
            u32::from_be_bytes([nal.data[0], nal.data[1], nal.data[2], nal.data[3]]),
            expected_len
        );
    }

    #[test]
    fn test_parse_sps_nal() {
        let mut parser = RtpParser::new();
        let ts = 0;
        let mut payload = rtp_header(ts);

        // SPS (type 7)
        payload.push(0x67);
        payload.extend_from_slice(b"sps_data");

        let nal = parser.parse(&payload, ts).expect("should parse SPS");
        assert_eq!(nal.nal_type, 7);
        assert!(!nal.is_keyframe);
    }

    #[test]
    fn test_parse_pps_nal() {
        let mut parser = RtpParser::new();
        let ts = 0;
        let mut payload = rtp_header(ts);

        // PPS (type 8)
        payload.push(0x68);
        payload.extend_from_slice(b"pps_data");

        let nal = parser.parse(&payload, ts).expect("should parse PPS");
        assert_eq!(nal.nal_type, 8);
    }

    #[test]
    fn test_fu_a_reassembly() {
        let mut parser = RtpParser::new();
        let ts = 90_000;

        // Build a fake IDR NAL split across 3 FU-A packets
        let original_nal: Vec<u8> = (0..50).collect(); // data 0..50

        // FU-A start packet
        let _fu_header_start = 0x25; // F=0, NRI=2, type=5 → NAL header for IDR
        let fu_indicator_start = 0x80 | 5; // S=1, type=5
        let mut pkt1 = rtp_header(ts);
        pkt1.push(0x7C); // F=0, NRI=2, type=28 (FU-A) in NAL header
        pkt1.push(fu_indicator_start);
        pkt1.extend_from_slice(&original_nal[0..20]);

        // FU-A continuation
        let fu_indicator_cont = 0x00 | 5; // S=0, E=0, type=5
        let mut pkt2 = rtp_header(ts);
        pkt2.push(0x7C);
        pkt2.push(fu_indicator_cont);
        pkt2.extend_from_slice(&original_nal[20..40]);

        // FU-A end
        let fu_indicator_end = 0x40 | 5; // E=1, type=5
        let mut pkt3 = rtp_header(ts);
        pkt3.push(0x7C);
        pkt3.push(fu_indicator_end);
        pkt3.extend_from_slice(&original_nal[40..]);

        assert!(parser.parse(&pkt1, ts).is_none()); // Start: no output yet
        assert!(parser.parse(&pkt2, ts).is_none()); // Continuation: no output
        let nal = parser.parse(&pkt3, ts).expect("should emit on end");

        assert_eq!(nal.nal_type, 5);
        assert!(nal.is_keyframe);

        // AVCC format: 4-byte len + reconstructed NAL
        // The reconstruction prepends a NAL header byte (0x25 for this IDR)
        let expected_len = 1 + original_nal.len(); // 1 header byte + data
        let len_prefix =
            u32::from_be_bytes([nal.data[0], nal.data[1], nal.data[2], nal.data[3]]) as usize;
        assert_eq!(len_prefix, expected_len);
        // nal.data[4] is the reconstructed NAL header: NRI from FU indicator (0x7C → NRI=3)
        // combined with the FU type (5) → 0x65
        assert_eq!(nal.data[4], 0x65);
        assert_eq!(&nal.data[5..], original_nal.as_slice());
    }

    #[test]
    fn test_fu_a_unexpected_continuation() {
        let mut parser = RtpParser::new();
        let ts = 0;

        // Continuation without start
        let mut pkt = rtp_header(ts);
        pkt.push(0x7C); // FU-A NAL header
        pkt.push(0x05); // S=0, E=0, type=5

        assert!(parser.parse(&pkt, ts).is_none()); // Should be ignored
        assert!(parser.fu_buffer.is_empty());
    }

    #[test]
    fn test_strip_start_code_4byte() {
        let data = [0x00, 0x00, 0x00, 0x01, 0xAA, 0xBB];
        assert_eq!(strip_start_code(&data), &[0xAA, 0xBB]);
    }

    #[test]
    fn test_strip_start_code_3byte() {
        let data = [0x00, 0x00, 0x01, 0xAA, 0xBB];
        assert_eq!(strip_start_code(&data), &[0xAA, 0xBB]);
    }

    #[test]
    fn test_strip_start_code_no_prefix() {
        let data = [0xAA, 0xBB];
        assert_eq!(strip_start_code(&data), &[0xAA, 0xBB]);
    }

    #[test]
    fn test_to_avcc_format() {
        let nal = [0x67, 0x42, 0x00, 0x1E]; // SPS start
        let avcc = to_avcc_format(&nal);
        assert_eq!(avcc.len(), nal.len() + 4);
        assert_eq!(
            u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]),
            nal.len() as u32
        );
        assert_eq!(&avcc[4..], &nal);
    }

    #[test]
    fn test_payload_too_short() {
        let mut parser = RtpParser::new();
        let payload = vec![0x80]; // Only 1 byte
        assert!(parser.parse(&payload, 0).is_none());
    }

    /// Hostile stream (Phase 2): a start bit followed by endless
    /// continuation fragments and no end bit. The reassembly buffer must
    /// hit the cap and reset instead of growing without bound, and the
    /// parser must recover for subsequent well-formed units.
    #[test]
    fn test_fu_a_overflow_resets_and_recovers() {
        let mut parser = RtpParser::new();

        // Start fragment (S=1, E=0, type=5).
        let mut start = rtp_header(0);
        start.push(0x7C);
        start.push(0x85);
        start.extend_from_slice(&[0u8; 100]);
        assert!(parser.parse(&start, 0).is_none());
        assert!(parser.fu_active);

        // Continuation fragments (~64 KiB each): enough to cross the
        // 1 MiB cap many times over. (Heap chunk: a 64 KiB stack array
        // per iteration would be wasteful.)
        let chunk = vec![0u8; 65_535];
        let mut cont = rtp_header(0);
        cont.push(0x7C);
        cont.push(0x05); // S=0, E=0, type=5
        cont.extend_from_slice(&chunk);
        for _ in 0..64 {
            assert!(parser.parse(&cont, 0).is_none());
        }

        // After the cap is exceeded, reassembly must have been reset.
        assert!(!parser.fu_active);
        assert!(parser.fu_buffer.is_empty());

        // Recovery: a small, well-formed FU-A unit still parses.
        let mut s = rtp_header(0);
        s.push(0x7C);
        s.push(0x85); // S=1, E=0
        s.extend_from_slice(&[1, 2, 3]);
        assert!(parser.parse(&s, 0).is_none());

        let mut e = rtp_header(0);
        e.push(0x7C);
        e.push(0x45); // S=0, E=1
        e.extend_from_slice(&[4, 5]);
        let nal = parser.parse(&e, 0).expect("parser recovers after overflow");
        assert_eq!(nal.nal_type, 5);
    }
}
