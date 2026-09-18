/// FLV (Flash Video) muxer: converts H.264 NAL units into FLV tags.
///
/// ## FLV File Structure
///
/// ```text
/// FLV Header (9 bytes):
///   Signature: "FLV" (0x46 0x4C 0x56)
///   Version: 1
///   TypeFlags: 0x05 (audio+video)
///   DataOffset: 9
///
/// PreviousTagSize0: 0 (4 bytes, big-endian)
/// [Tag1] [PreviousTagSize1 (4 bytes)]
/// [Tag2] [PreviousTagSize2 (4 bytes)]
/// ...
/// ```
///
/// ## Video Tag Structure
///
/// ```text
/// Tag header (11 bytes):
///   TagType: 0x09 (Video)
///   DataSize: 3 bytes BE
///   Timestamp: 3 bytes BE (lower 24 bits)
///   TimestampExtended: 1 byte (high 8 bits)
///   StreamID: 3 bytes (always 0)
///
/// Video tag data:
///   FrameType(4 bits) | CodecID(4 bits)
///     1 = keyframe, 2 = inter frame
///     7 = AVC
///   AVCPacketType: 1 byte
///     0 = AVC sequence header (SPS+PPS)
///     1 = AVC NAL unit
///     2 = AVC end of sequence
///   CompositionTime: 3 bytes (signed, 0 for live)
///   Data: NAL unit(s)
/// ```

// --- NAL type constants ---
const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;
// const NAL_TYPE_IDR: u8 = 5;

// --- FLV constants ---
const FLV_HEADER: &[u8; 9] = b"FLV\x01\x01\x00\x00\x00\x09";
const FLV_PREV_TAG_SIZE_ZERO: &[u8; 4] = &[0x00, 0x00, 0x00, 0x00];
const TAG_TYPE_VIDEO: u8 = 0x09;
const TAG_TYPE_SCRIPT: u8 = 0x12;
const CODEC_AVC: u8 = 7;
const FRAME_KEYFRAME: u8 = 1;
const FRAME_INTER: u8 = 2;
const AVC_SEQUENCE_HEADER: u8 = 0;
const AVC_NALU: u8 = 1;

/// Build the FLV file header (always 9 bytes).
///
/// This is sent first to every new FLV client so flv.js can
/// validate the stream format.
#[must_use]
pub fn flv_header() -> Vec<u8> {
    let mut buf = Vec::with_capacity(13);
    buf.extend_from_slice(FLV_HEADER);
    buf.extend_from_slice(FLV_PREV_TAG_SIZE_ZERO);
    buf
}

/// Build the FLV script tag with onMetaData.
#[must_use]
pub fn flv_script_tag() -> Vec<u8> {
    build_script_tag(0)
}

/// Build the FLV script tag with a specific timestamp.
#[must_use]
fn build_script_tag(timestamp: u32) -> Vec<u8> {
    let metadata = build_on_metadata();
    let data_size = metadata.len() as u32;

    let mut tag = Vec::with_capacity(11 + metadata.len() + 4);
    tag.extend_from_slice(&build_tag_header(TAG_TYPE_SCRIPT, data_size, timestamp));
    tag.extend_from_slice(&metadata);
    tag.extend_from_slice(&(11u32 + data_size).to_be_bytes()); // PreviousTagSize (11-byte header + data)
    tag
}

/// FLV muxer state machine.
///
/// Accumulates SPS/PPS from the stream to build the AVC decoder
/// configuration record, then emits FLV tags for video frames.
pub struct FlvMuxer {
    /// Accumulated SPS NAL unit (AVCC format, without 4-byte length prefix).
    sps: Option<Vec<u8>>,
    /// Accumulated PPS NAL unit (AVCC format, without 4-byte length prefix).
    pps: Option<Vec<u8>>,
    /// Whether the FLV header + script tag + sequence header have been emitted.
    initialized: bool,
    /// Start time for initialization tag timestamps.
    start_time: Option<std::time::Instant>,
    /// Timestamp baseline for video frames (set on first video frame).
    video_base_ms: Option<u32>,
    /// Sequence header tag bytes, cached so new clients receive it on connect.
    sequence_header_tag: Option<Vec<u8>>,
}

impl FlvMuxer {
    /// Create a new FLV muxer with no accumulated state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sps: None,
            pps: None,
            initialized: false,
            start_time: None,
            video_base_ms: None,
            sequence_header_tag: None,
        }
    }

    /// Process a NAL unit and return any FLV tags that should be emitted.
    ///
    /// For SPS/PPS: accumulates them for the sequence header.
    /// For I/P frames: emits video tags.
    ///
    /// The first time both SPS and PPS are available, also emits a complete
    /// initialization burst: FLV header → script tag → sequence header.
    /// After that, only video frame tags are emitted.
    ///
    /// Returns a vector of FLV tag byte buffers, which may be empty if
    /// the NAL was consumed for accumulation (SPS/PPS).
    pub fn process_nal(&mut self, nal: &crate::video::rtp_parser::NalUnit) -> Vec<Vec<u8>> {
        let mut tags = Vec::new();

        match nal.nal_type {
            NAL_TYPE_SPS => {
                // Set initialization timebase on first SPS
                if self.start_time.is_none() {
                    self.start_time = Some(std::time::Instant::now());
                }
                let raw = if nal.data.len() > 4 {
                    &nal.data[4..]
                } else {
                    &nal.data
                };
                self.sps = Some(raw.to_vec());
                if self.pps.is_some() && !self.initialized {
                    self.emit_initialization(&mut tags);
                }
            }
            NAL_TYPE_PPS => {
                let raw = if nal.data.len() > 4 {
                    &nal.data[4..]
                } else {
                    &nal.data
                };
                self.pps = Some(raw.to_vec());
                if self.sps.is_some() && !self.initialized {
                    self.emit_initialization(&mut tags);
                }
            }
            _ => {
                if self.initialized {
                    // Set video timebase on first video frame so timestamp starts at 0
                    if self.video_base_ms.is_none() {
                        self.video_base_ms = Some(self.elapsed_ms());
                    }
                    let ts = self.elapsed_ms() - self.video_base_ms.unwrap_or(0);
                    let tag = self.build_video_tag(&nal.data, nal.is_keyframe, ts);
                    tags.push(tag);
                } else {
                    tracing::trace!(
                        "FLV: deferring frame type={} until SPS/PPS received",
                        nal.nal_type
                    );
                }
            }
        }

        tags
    }

    /// Get the cached sequence header tag for new client catch-up.
    #[must_use]
    pub fn sequence_header(&self) -> Option<&[u8]> {
        self.sequence_header_tag.as_deref()
    }

    // --- Private helpers ---

    fn elapsed_ms(&self) -> u32 {
        self.start_time
            .map(|t| t.elapsed().as_millis() as u32)
            .unwrap_or(0)
    }

    /// Emit the initialization burst: script tag + sequence header.
    fn emit_initialization(&mut self, tags: &mut Vec<Vec<u8>>) {
        let ts = self.elapsed_ms();
        // Script metadata tag
        tags.push(build_script_tag(ts));

        // AVC sequence header (SPS + PPS)
        let seq_tag = self.build_sequence_header_with_ts(ts);
        tags.push(seq_tag.clone());
        self.sequence_header_tag = Some(seq_tag);

        self.initialized = true;
        tracing::info!("FLV: initialization complete (script + sequence header emitted)");
    }

    /// Build AVC sequence header tag (called with timestamp).
    fn build_sequence_header_with_ts(&self, timestamp: u32) -> Vec<u8> {
        let sps = self.sps.as_deref().unwrap_or(&[]);
        let pps = self.pps.as_deref().unwrap_or(&[]);

        // Extract profile/level from SPS if available.
        // SPS: byte 0=NAL header, 1=profile_idc, 2=constraint flags, 3=level_idc
        let (profile, level) = if sps.len() >= 4 {
            (sps[1], sps[3])
        } else {
            (0x42, 0x1E) // Baseline 4.2 defaults
        };

        let config = {
            let mut c = Vec::with_capacity(7 + sps.len() + pps.len());
            c.push(1); // version
            c.push(profile);
            c.push(0); // compatibility
            c.push(level);
            c.push(0xFF); // lengthSizeMinusOne=3, reserved bits
            c.push(0xE1); // numSPS=1, reserved bits
            c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
            c.extend_from_slice(sps);
            c.push(0x01); // numPPS=1
            c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
            c.extend_from_slice(pps);
            c
        };

        self.build_video_frame_tag(FRAME_KEYFRAME, AVC_SEQUENCE_HEADER, &config, timestamp)
    }

    /// Build a video tag from raw NAL data (AVCC format).
    fn build_video_tag(&self, nal_data: &[u8], is_keyframe: bool, timestamp: u32) -> Vec<u8> {
        let frame_type = if is_keyframe {
            FRAME_KEYFRAME
        } else {
            FRAME_INTER
        };
        self.build_video_frame_tag(frame_type, AVC_NALU, nal_data, timestamp)
    }

    /// Build a video frame tag with the specified parameters.
    fn build_video_frame_tag(
        &self,
        frame_type: u8,
        avc_packet_type: u8,
        data: &[u8],
        timestamp: u32,
    ) -> Vec<u8> {
        // Video data: FrameType|CodecID + AVCPacketType + CompositionTime + raw
        let frame_type_codec = (frame_type << 4) | CODEC_AVC;
        let video_data_len = 1 // frame_type|codedID
            + 1 // AVCPacketType
            + 3 // CompositionTime (always 0 for live)
            + data.len();

        let mut tag = Vec::with_capacity(11 + video_data_len + 4);

        // Tag header
        tag.extend_from_slice(&build_tag_header(
            TAG_TYPE_VIDEO,
            video_data_len as u32,
            timestamp,
        ));

        // Video tag data
        tag.push(frame_type_codec);
        tag.push(avc_packet_type);
        tag.extend_from_slice(&[0x00, 0x00, 0x00]); // CompositionTime = 0
        tag.extend_from_slice(data);

        // PreviousTagSize: 11-byte header + video data
        let prev_size = (11 + video_data_len) as u32;
        tag.extend_from_slice(&prev_size.to_be_bytes());

        tag
    }
}

/// Build an 11-byte FLV tag header.
fn build_tag_header(tag_type: u8, data_size: u32, timestamp: u32) -> [u8; 11] {
    let mut hdr = [0u8; 11];
    hdr[0] = tag_type;
    // DataSize: 3 bytes BE
    hdr[1] = ((data_size >> 16) & 0xFF) as u8;
    hdr[2] = ((data_size >> 8) & 0xFF) as u8;
    hdr[3] = (data_size & 0xFF) as u8;
    // Timestamp: 3 bytes BE (lower 24 bits)
    hdr[4] = ((timestamp >> 16) & 0xFF) as u8;
    hdr[5] = ((timestamp >> 8) & 0xFF) as u8;
    hdr[6] = (timestamp & 0xFF) as u8;
    // TimestampExtended: 1 byte (high 8 bits)
    hdr[7] = ((timestamp >> 24) & 0xFF) as u8;
    // StreamID: 3 bytes (always 0)
    hdr[8] = 0;
    hdr[9] = 0;
    hdr[10] = 0;
    hdr
}

/// Build a minimal onMetaData AMF0 object for flv.js.
///
/// This is a hand-crafted AMF0 encoded object. While not exhaustive,
/// it provides enough information for flv.js to recognize the stream
/// as an FLV video stream and begin decoding.
fn build_on_metadata() -> Vec<u8> {
    // AMF0 string "onMetaData" (type 0x02)
    // + AMF0 ECMA array (type 0x08) with minimal fields
    let fields: &[(&str, f64)] = &[
        ("duration", 0.0),
        ("width", 2560.0),
        ("height", 1440.0),
        ("videodatarate", 0.0),
        ("framerate", 15.0),
        ("videocodecid", 7.0), // AVC
        ("encoder", 0.0),
        ("filesize", 0.0),
    ];

    // AMF0 string
    let script_name = "onMetaData";
    let mut buf = Vec::with_capacity(256);
    buf.push(0x02); // AMF0 type: string
    buf.extend_from_slice(&(script_name.len() as u16).to_be_bytes());
    buf.extend_from_slice(script_name.as_bytes());

    // AMF0 ECMA array (type 0x08)
    buf.push(0x08);
    buf.extend_from_slice(&(fields.len() as u32).to_be_bytes());

    for &(name, value) in fields {
        // Property name: AMF0 string (implicit in ECMA array, just length + data)
        buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
        buf.extend_from_slice(name.as_bytes());
        // Value: AMF0 number (type 0x00)
        buf.push(0x00);
        buf.extend_from_slice(&value.to_be_bytes());
    }

    // End of ECMA array marker: name-length=0 + object-end-marker (0x09)
    buf.extend_from_slice(&[0x00, 0x00, 0x09]);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flv_header() {
        let hdr = flv_header();
        assert_eq!(hdr.len(), 13);
        assert_eq!(&hdr[0..3], b"FLV");
        assert_eq!(hdr[3], 1); // version
        assert_eq!(hdr[4], 0x01); // video-only flags
                                  // DataOffset: 9
        assert_eq!(u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]), 9);
    }

    #[test]
    fn test_build_tag_header() {
        let hdr = build_tag_header(TAG_TYPE_VIDEO, 100, 0);
        assert_eq!(hdr[0], 0x09); // Video tag type
        assert_eq!(hdr[1..4], [0x00, 0x00, 0x64]); // DataSize=100 BE
        assert_eq!(hdr[4..7], [0x00, 0x00, 0x00]); // Timestamp=0
        assert_eq!(hdr[7], 0x00); // TimestampExtended
        assert_eq!(hdr[8..11], [0x00, 0x00, 0x00]); // StreamID=0

        // Test timestamp with high bit
        let hdr_high = build_tag_header(TAG_TYPE_VIDEO, 100, 0x0100_0000);
        assert_eq!(hdr_high[7], 0x01); // TimestampExtended
    }

    #[test]
    fn test_muxer_initial_state() {
        let muxer = FlvMuxer::new();
        assert!(muxer.sps.is_none());
        assert!(muxer.pps.is_none());
        assert!(!muxer.initialized);
    }

    #[test]
    fn test_build_on_metadata() {
        let meta = build_on_metadata();
        // Should start with AMF0 string marker + "onMetaData"
        assert_eq!(meta[0], 0x02);
        let name_len = u16::from_be_bytes([meta[1], meta[2]]) as usize;
        assert_eq!(&meta[3..3 + name_len], b"onMetaData");
        // Should continue with ECMA array marker
        let after_name = 3 + name_len;
        assert_eq!(meta[after_name], 0x08);
    }

    // --- Canned NAL units (the muxer does not decode them; only the NAL
    // type, keyframe flag, and byte length matter) ---

    fn sps_nal() -> crate::video::rtp_parser::NalUnit {
        // 4-byte AVCC length prefix + a real SPS (NAL hdr, profile, constr, level).
        crate::video::rtp_parser::NalUnit {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, 0x28, 0x96],
            nal_type: 7,
            is_keyframe: false,
            timestamp: 0,
        }
    }

    fn pps_nal() -> crate::video::rtp_parser::NalUnit {
        crate::video::rtp_parser::NalUnit {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80],
            nal_type: 8,
            is_keyframe: false,
            timestamp: 0,
        }
    }

    fn idr_nal() -> crate::video::rtp_parser::NalUnit {
        crate::video::rtp_parser::NalUnit {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC],
            nal_type: 5,
            is_keyframe: true,
            timestamp: 0,
        }
    }

    fn pframe_nal() -> crate::video::rtp_parser::NalUnit {
        crate::video::rtp_parser::NalUnit {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x11, 0x22],
            nal_type: 1,
            is_keyframe: false,
            timestamp: 0,
        }
    }

    /// Read the FLV tag timestamp (24-bit + 8-bit ext) from a tag's header.
    fn tag_ts(tag: &[u8]) -> u32 {
        ((tag[4] as u32) << 16) | ((tag[5] as u32) << 8) | (tag[6] as u32) | ((tag[7] as u32) << 24)
    }

    /// Drive SPS then PPS and return the initialization burst.
    fn init_burst(muxer: &mut FlvMuxer) -> Vec<Vec<u8>> {
        assert!(muxer.process_nal(&sps_nal()).is_empty(), "SPS alone does not init");
        muxer.process_nal(&pps_nal())
    }

    #[test]
    fn init_burst_is_script_then_sequence_header() {
        let mut m = FlvMuxer::new();
        let burst = init_burst(&mut m);
        assert_eq!(burst.len(), 2, "exactly script + sequence header");

        // Tag 1: script (onMetaData).
        let script = &burst[0];
        assert_eq!(script[0], TAG_TYPE_SCRIPT);
        assert!(script.windows(10).any(|w| w == b"onMetaData"));

        // Tag 2: AVC sequence header video tag.
        let seq = &burst[1];
        assert_eq!(seq[0], TAG_TYPE_VIDEO);
        assert_eq!(seq[11], FRAME_KEYFRAME << 4 | CODEC_AVC, "keyframe|AVC");
        assert_eq!(seq[12], AVC_SEQUENCE_HEADER);
        assert_eq!(&seq[13..16], &[0, 0, 0], "composition time");
        // AVC decoder config record.
        assert_eq!(seq[16], 1, "config version");
        assert_eq!(seq[17], 0x42, "profile from SPS[1]");
        assert_eq!(seq[18], 0, "compatibility");
        assert_eq!(seq[19], 0x1E, "level from SPS[3]");
        assert_eq!(seq[20], 0xFF);
        assert_eq!(seq[21], 0xE1, "numSPS=1");
    }

    #[test]
    fn sequence_header_emitted_exactly_once() {
        let mut m = FlvMuxer::new();
        init_burst(&mut m);

        // Video frames after init emit exactly one tag each (no re-init).
        let t1 = m.process_nal(&idr_nal());
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0][12], AVC_NALU);

        // Re-feeding SPS/PPS after init is a no-op (no initialization burst).
        assert!(m.process_nal(&sps_nal()).is_empty());
        assert!(m.process_nal(&pps_nal()).is_empty());
    }

    #[test]
    fn pps_then_sps_still_initializes() {
        // PPS arriving before SPS must not init; SPS then completes it.
        let mut m = FlvMuxer::new();
        assert!(m.process_nal(&pps_nal()).is_empty());
        let burst = m.process_nal(&sps_nal());
        assert_eq!(burst.len(), 2, "SPS after PPS triggers init");
        assert_eq!(burst[1][0], TAG_TYPE_VIDEO);
    }

    #[test]
    fn idr_vs_pframe_keyframe_flag() {
        let mut m = FlvMuxer::new();
        init_burst(&mut m);

        let idr = &m.process_nal(&idr_nal())[0];
        let p = &m.process_nal(&pframe_nal())[0];
        // FrameType occupies the high nibble of byte 11.
        assert_eq!(idr[11] >> 4, FRAME_KEYFRAME);
        assert_eq!(p[11] >> 4, FRAME_INTER);
        // Both are AVC NAL units with the codec nibble preserved.
        assert_eq!(idr[11] & 0x0F, CODEC_AVC);
        assert_eq!(p[11] & 0x0F, CODEC_AVC);
        assert_eq!(idr[12], AVC_NALU);
        assert_eq!(p[12], AVC_NALU);
    }

    #[test]
    fn video_frame_deferred_before_sequence_header() {
        // A frame arriving before SPS/PPS is held, not emitted.
        let mut m = FlvMuxer::new();
        assert!(m.process_nal(&idr_nal()).is_empty(), "pre-init frame deferred");
        // It does not clobber the not-yet-set state.
        assert!(m.sps.is_none() && m.pps.is_none() && !m.initialized);
    }

    #[test]
    fn sequence_header_cached_for_catchup() {
        let mut m = FlvMuxer::new();
        let burst = init_burst(&mut m);
        let cached = m.sequence_header().expect("sequence header cached");
        assert_eq!(cached, burst[1].as_slice());
    }

    #[test]
    fn sequence_header_none_before_init() {
        // New muxer has no sequence header until SPS+PPS arrive.
        assert!(FlvMuxer::new().sequence_header().is_none());
    }

    #[test]
    fn flv_script_tag_is_metadata_at_zero() {
        let tag = flv_script_tag();
        assert_eq!(tag[0], TAG_TYPE_SCRIPT);
        assert!(tag.windows(10).any(|w| w == b"onMetaData"));
    }

    #[test]
    fn short_nal_without_start_code_prefix() {
        // NALs whose data is <= 4 bytes skip the AVCC-prefix strip (the
        // `else` arm of the SPS/PPS accumulation).
        let mut m = FlvMuxer::new();
        let short_sps = crate::video::rtp_parser::NalUnit {
            data: vec![0x67, 0x42, 0x00, 0x1E], // len == 4, no prefix stripped
            nal_type: 7,
            is_keyframe: false,
            timestamp: 0,
        };
        let short_pps = crate::video::rtp_parser::NalUnit {
            data: vec![0x68, 0xCE], // len < 4
            nal_type: 8,
            is_keyframe: false,
            timestamp: 0,
        };
        assert!(m.process_nal(&short_sps).is_empty());
        let burst = m.process_nal(&short_pps);
        assert_eq!(burst.len(), 2, "init still fires without a start-code prefix");
    }

    #[test]
    fn short_sps_falls_back_to_default_profile_level() {
        // raw SPS < 4 bytes (after the 4-byte prefix) -> defaults (0x42, 0x1E).
        let mut m = FlvMuxer::new();
        let short_sps = crate::video::rtp_parser::NalUnit {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x99], // raw = 2 bytes
            nal_type: 7,
            is_keyframe: false,
            timestamp: 0,
        };
        assert!(m.process_nal(&short_sps).is_empty());
        let burst = m.process_nal(&pps_nal());
        let seq = &burst[1];
        assert_eq!(seq[17], 0x42, "default profile");
        assert_eq!(seq[19], 0x1E, "default level");
    }

    #[test]
    fn video_timestamps_are_monotonic_from_zero() {
        let mut m = FlvMuxer::new();
        init_burst(&mut m);
        // The muxer derives timestamps from the wall clock (input NALs carry
        // none), so assert the invariant that matters: the baseline is pinned
        // at the first frame and subsequent timestamps never go backwards.
        let mut prev_ts = tag_ts(&m.process_nal(&idr_nal())[0]);
        for _ in 0..8 {
            let ts = tag_ts(&m.process_nal(&pframe_nal())[0]);
            assert!(ts >= prev_ts, "timestamp went backwards: {prev_ts} -> {ts}");
            prev_ts = ts;
        }
        // All within the first couple of seconds of the stream.
        assert!(prev_ts < 1000);
    }
}
