/// Video streaming module.
///
/// Implements the RTSP → HTTP-FLV pipeline:
///   Camera RTSP → RTSP Client → RTP Parser → FLV Muxer → HTTP Server → Browser (flv.js)
///
/// ## Architecture
///
/// ```text
/// RTSP Client (reads $<channel><len><payload> from TCP)
///     ↓ (mpsc: tagged RTP packets — video ch 0, audio ch 2)
/// Pipeline Task ─┬─ video: RTP parser → NAL → FLV tags → stream1 broadcast
///                └─ audio: G.711 decode → AAC → FLV tags → stream2 broadcast
///     ↓ (broadcast: FLV tag byte vectors to HTTP clients)
/// HTTP Server (hyper on the session's own livePort, see `ports::PortPool`)
///     ↓ (HTTP response body streaming FLV tags)
/// Browser (flv.js player)
/// ```
///
/// One `VideoServer` is instantiated **per logged-in session**: the core
/// handler owns it (spawning/tearing down the pipeline on video
/// open/close) and shares it with that session's own HTTP-FLV listener
/// (which serves the playlist + stream endpoints on the session's
/// allocated port). Multiple sessions run side by side with no shared
/// video state.
pub mod flv_muxer;
pub mod http_flv;
pub mod ports;
pub mod rtp_parser;
pub mod rtsp_client;

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

use crate::net::OriginInfo;

/// Per-session video streaming state.
///
/// One instance per logged-in session, shared between the core WebSocket
/// handler (which spawns/teardowns the pipeline on video open/close) and
/// the session's HTTP-FLV listener. Thread-safe: all fields behind `Arc`
/// + appropriate locks.
#[derive(Debug)]
pub struct VideoServer {
    /// Broadcast sender for video FLV tags. `None` when no video pipeline is active.
    video_tx: RwLock<Option<broadcast::Sender<Vec<u8>>>>,
    /// Broadcast sender for audio FLV tags (`stream2.flv`). `None` when
    /// no pipeline is active.
    audio_tx: RwLock<Option<broadcast::Sender<Vec<u8>>>>,
    /// Handle to the running pipeline task, used to abort on close.
    video_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Origin of the camera whose stream is (or was) being served
    /// (`http://<claimed-host>:<webPort>`). The stock UI page is served by
    /// the camera, so its `Origin` when fetching FLV is exactly this —
    /// the HTTP-FLV server uses it to gate the stream endpoints against
    /// unrelated websites. `None` when no pipeline has been started.
    armed_origin: RwLock<Option<OriginInfo>>,
}

impl VideoServer {
    /// Create a new shared video server with no active pipeline.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            video_tx: RwLock::new(None),
            audio_tx: RwLock::new(None),
            video_task: Mutex::new(None),
            armed_origin: RwLock::new(None),
        })
    }

    /// The camera origin the FLV endpoints may serve (see `armed_origin`).
    pub fn armed_origin(&self) -> Option<OriginInfo> {
        self.armed_origin.try_read().ok()?.clone()
    }

    /// Decide whether an HTTP request may consume the live streams.
    ///
    /// - No pipeline armed → allow (nothing is live; stream endpoints 404).
    /// - No `Origin` header → allow: browsers always send `Origin` on
    ///   cross-origin XHR/fetch, so an absent header means a CLI client
    ///   (curl/ffprobe troubleshooting, see README).
    /// - `Origin` present → must parse and match the armed camera origin.
    ///   `Origin: null` (sandboxed iframe) parses to `None` → denied.
    pub fn origin_allowed(&self, origin_header: Option<&str>) -> bool {
        let Some(armed) = self.armed_origin() else {
            return true;
        };
        match origin_header {
            None => true,
            Some(raw) => OriginInfo::parse(raw)
                .is_some_and(|o| o.matches(&armed.host, armed.port)),
        }
    }

    /// Subscribe to the video FLV tag broadcast.
    ///
    /// Returns `None` if no video pipeline is currently active.
    /// Use before connecting to stream or polling for updates.
    #[allow(dead_code)]
    pub fn subscribe_video(&self) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.video_tx
            .try_read()
            .ok()
            .and_then(|tx| tx.as_ref().map(broadcast::Sender::subscribe))
    }

    /// Subscribe to the audio FLV tag broadcast.
    ///
    /// Returns `None` if no pipeline is currently active.
    #[allow(dead_code)]
    pub fn subscribe_audio(&self) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.audio_tx
            .try_read()
            .ok()
            .and_then(|tx| tx.as_ref().map(broadcast::Sender::subscribe))
    }

    /// Check if a video pipeline is active without subscribing.
    #[allow(dead_code)]
    pub fn has_video(&self) -> bool {
        self.video_tx.try_read().ok().is_some_and(|tx| tx.is_some())
    }

    /// Start the video pipeline for a camera stream.
    ///
    /// Spawns the RTSP client + RTP parser + FLV muxer pipeline in a Tokio task.
    /// Sets up the broadcast channel so HTTP clients can subscribe.
    ///
    /// `origin` is the *claimed* camera host and HTTP port (not the dial
    /// address): it identifies the page origin that is allowed to fetch the
    /// FLV streams (see `origin_allowed`).
    pub async fn start(
        self: &Arc<Self>,
        camera_ip: &str,
        rtsp_port: u16,
        stream_type: u32,
        user: &str,
        pwd: &str,
        origin: &OriginInfo,
    ) -> Result<(), anyhow::Error> {
        // Stop any existing pipeline first
        self.stop().await;

        let path = match stream_type {
            1 => "videoSub",
            _ => "videoMain",
        };

        // Pin the allowed FLV-fetch origin for the duration of the pipeline.
        *self.armed_origin.write().await = Some(origin.clone());

        let (video_broadcast_tx, _) = broadcast::channel::<Vec<u8>>(64);
        let (audio_broadcast_tx, _) = broadcast::channel::<Vec<u8>>(64);
        *self.video_tx.write().await = Some(video_broadcast_tx.clone());
        *self.audio_tx.write().await = Some(audio_broadcast_tx.clone());

        let camera_ip = camera_ip.to_string();
        let user = user.to_string();
        let pwd = pwd.to_string();

        let task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            tracing::info!(
                "Video pipeline starting: {}:{}/{path} stream_type={stream_type}",
                camera_ip,
                rtsp_port,
            );

            let (rtp_tx, rtp_rx) = mpsc::channel::<StreamPacket>(128);
            if let Err(e) = run_pipeline(
                &camera_ip,
                rtsp_port,
                path,
                &user,
                &pwd,
                rtp_tx,
                rtp_rx,
                video_broadcast_tx.clone(),
                audio_broadcast_tx.clone(),
            )
            .await
            {
                tracing::error!("Video pipeline error: {e}");
            }
            tracing::info!("Video pipeline stopped");
        });

        *self.video_task.lock().await = Some(task);

        Ok(())
    }

    /// Stop the active video pipeline.
    ///
    /// Aborts the pipeline task and clears the broadcast channel.
    /// Safe to call when no pipeline is active.
    pub async fn stop(&self) {
        // Drop the broadcast senders so subscribers see the stream end.
        *self.video_tx.write().await = None;
        *self.audio_tx.write().await = None;
        *self.armed_origin.write().await = None;

        // Abort the pipeline task if running.
        if let Some(handle) = self.video_task.lock().await.take() {
            handle.abort();
            tracing::debug!("Video pipeline task aborted");
        }
    }
}

/// An RTP packet from the RTSP reader, tagged by track.
enum StreamPacket {
    /// Video track (interleaved channel 0).
    Video(Vec<u8>),
    /// Audio track (interleaved channel 2).
    Audio(Vec<u8>),
}

/// The main RTSP → RTP → FLV pipeline.
///
/// Runs inside a spawned Tokio task. Reads RTP data from the camera's
/// RTSP TCP socket. The video track is parsed into NAL units and
/// muxed to FLV video tags (`stream1.flv`); the audio track is
/// G.711-decoded, AAC-encoded, and muxed to FLV audio tags
/// (`stream2.flv`). Both are broadcast to HTTP clients.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    camera_ip: &str,
    rtsp_port: u16,
    path: &str,
    user: &str,
    pwd: &str,
    rtp_tx: mpsc::Sender<StreamPacket>,
    mut rtp_rx: mpsc::Receiver<StreamPacket>,
    video_broadcast_tx: broadcast::Sender<Vec<u8>>,
    audio_broadcast_tx: broadcast::Sender<Vec<u8>>,
) -> Result<(), anyhow::Error> {
    use crate::audio::transcode::AudioTranscoder;
    use flv_muxer::FlvMuxer;
    use rtp_parser::RtpParser;
    use rtsp_client::{RtpPacket, RtspClient};

    // Phase 2: Connect to RTSP

    let rtsp_url = format!("rtsp://{camera_ip}:{rtsp_port}/{path}");
    tracing::info!("Connecting to RTSP: {rtsp_url}");

    let mut rtsp = RtspClient::connect(camera_ip, rtsp_port, path, user, pwd).await?;
    tracing::info!("RTSP handshake complete — starting RTP reader");

    // Spawn the RTSP reader that sends tagged RTP packets into the channel
    let reader_rtp_tx = rtp_tx;
    tokio::spawn(async move {
        let mut packet_count: u64 = 0;
        let mut audio_count: u64 = 0;
        loop {
            match rtsp.read_rtp_packet().await {
                Ok(RtpPacket::Video(payload)) => {
                    packet_count += 1;
                    if packet_count <= 5 || packet_count % 100 == 0 {
                        tracing::debug!(
                            "RTSP: received video packet #{packet_count}, {} bytes",
                            payload.len()
                        );
                    }
                    if reader_rtp_tx
                        .send(StreamPacket::Video(payload))
                        .await
                        .is_err()
                    {
                        tracing::debug!("RTSP: mpsc channel closed after {packet_count} packets");
                        break;
                    }
                }
                Ok(RtpPacket::Audio(payload)) => {
                    audio_count += 1;
                    if audio_count <= 5 || audio_count % 200 == 0 {
                        tracing::debug!(
                            "RTSP: received audio packet #{audio_count}, {} bytes",
                            payload.len()
                        );
                    }
                    if reader_rtp_tx
                        .send(StreamPacket::Audio(payload))
                        .await
                        .is_err()
                    {
                        tracing::debug!(
                            "RTSP: mpsc channel closed after {audio_count} audio packets"
                        );
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("RTSP read error after {packet_count} packets: {e}");
                    break;
                }
            }
        }
        tracing::debug!("RTSP reader task exiting");
    });

    // Phase 3 + 4: Parse RTP and mux to FLV
    let mut parser = RtpParser::new();
    let mut muxer = FlvMuxer::new();
    let mut audio = AudioTranscoder::new()?;

    let mut tag_count: u64 = 0;
    let mut audio_tag_count: u64 = 0;
    while let Some(pkt) = rtp_rx.recv().await {
        match pkt {
            StreamPacket::Video(rtp_payload) => {
                let timestamp = 0u32;

                if let Some(nal) = parser.parse(&rtp_payload, timestamp) {
                    let nal_type = nal.nal_type;
                    let is_keyframe = nal.is_keyframe;
                    let tags = muxer.process_nal(&nal);
                    for tag in tags {
                        tag_count += 1;
                        if tag_count <= 10 || tag_count % 30 == 0 {
                            tracing::debug!(
                                "FLV: tag #{tag_count}, {tag_len} bytes (NAL type={nal_type}, keyframe={is_keyframe})",
                                tag_len = tag.len()
                            );
                        }
                        let _ = video_broadcast_tx.send(tag);
                    }
                }
            }
            StreamPacket::Audio(rtp_payload) => match audio.process_rtp(&rtp_payload) {
                Ok(tags) => {
                    for tag in tags {
                        audio_tag_count += 1;
                        if audio_tag_count <= 10 || audio_tag_count % 50 == 0 {
                            tracing::debug!(
                                "FLV audio: tag #{audio_tag_count}, {} bytes",
                                tag.len()
                            );
                        }
                        let _ = audio_broadcast_tx.send(tag);
                    }
                }
                Err(e) => tracing::error!("audio transcode error: {e}"),
            },
        }
    }

    tracing::info!("Pipeline exiting — {tag_count} video tags, {audio_tag_count} audio tags sent");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::OriginInfo;

    fn origin() -> OriginInfo {
        OriginInfo {
            host: "camera.lan".into(),
            port: 88,
        }
    }

    #[tokio::test]
    async fn state_is_empty_before_start() {
        let vs = VideoServer::new();
        assert!(!vs.has_video());
        assert!(vs.subscribe_video().is_none());
        assert!(vs.subscribe_audio().is_none());
        assert!(vs.armed_origin().is_none());
        // Unarmed → origin gate is open (nothing live to protect).
        assert!(vs.origin_allowed(Some("http://evil.example")));
        assert!(vs.origin_allowed(None));
    }

    // --- End-to-end: RTSP → RTP → FLV pipeline against a mock camera ---

    fn rtp_header(seq: u16, ts: u32) -> Vec<u8> {
        let mut h = vec![0u8; 12];
        h[0] = 0x80; // version=2
        h[1] = 96; // payload type
        h[2..4].copy_from_slice(&seq.to_be_bytes());
        h[4..8].copy_from_slice(&ts.to_be_bytes());
        h[8..12].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        h
    }

    fn video_rtp(seq: u16, nal_hdr: u8, nal_data: &[u8]) -> Vec<u8> {
        let mut p = rtp_header(seq, 90_000);
        p.push(nal_hdr);
        p.extend_from_slice(nal_data);
        p
    }

    fn audio_rtp(seq: u16) -> Vec<u8> {
        let mut p = rtp_header(seq, 44_100);
        // μ-law-ish payload; content is irrelevant for coverage.
        p.extend_from_slice(&[0xFFu8; 160]);
        p
    }

    /// A one-shot mock RTSP camera: answers the handshake, then after PLAY
    /// streams the given interleaved `(channel, rtp_packet)` pairs and closes.
    async fn spawn_rtsp_mock(rtp: Vec<(u8, Vec<u8>)>) -> u16 {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = sock.into_split();
            let mut br = tokio::io::BufReader::new(r);
            loop {
                let mut line = String::new();
                if br.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let method = line.split_whitespace().next().unwrap_or("");
                loop {
                    let mut h = String::new();
                    if br.read_line(&mut h).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if h.trim().is_empty() {
                        break;
                    }
                }
                match method {
                    "OPTIONS" | "TEARDOWN" => {
                        w.write_all(b"RTSP/1.0 200 OK\r\n\r\n")
                            .await
                            .ok();
                        w.flush().await.ok();
                    }
                    "DESCRIBE" => {
                        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 RTP/AVP 96\r\n";
                        let resp = format!(
                            "RTSP/1.0 200 OK\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{}",
                            sdp.len(),
                            sdp
                        );
                        w.write_all(resp.as_bytes()).await.ok();
                        w.flush().await.ok();
                    }
                    "SETUP" => {
                        w.write_all(b"RTSP/1.0 200 OK\r\nSession: pipeline-sess\r\n\r\n")
                            .await
                            .ok();
                        w.flush().await.ok();
                    }
                    "PLAY" => {
                        w.write_all(b"RTSP/1.0 200 OK\r\nSession: pipeline-sess\r\n\r\n")
                            .await
                            .ok();
                        w.flush().await.ok();
                        if !rtp.is_empty() {
                            // Delay so the client's BufReader hasn't read ahead
                            // past the PLAY response into the RTP data.
                            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                            for (ch, payload) in &rtp {
                                let mut pkt = vec![
                                    b'$',
                                    *ch,
                                    (payload.len() >> 8) as u8,
                                    payload.len() as u8,
                                ];
                                pkt.extend_from_slice(payload);
                                w.write_all(&pkt).await.ok();
                            }
                            w.flush().await.ok();
                        }
                        return;
                    }
                    _ => {
                        w.write_all(b"RTSP/1.0 500 Error\r\n\r\n")
                            .await
                            .ok();
                        w.flush().await.ok();
                    }
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn pipeline_streams_rtp_to_broadcast() {
        // SPS, PPS, IDR video NALs (single-NAL RTP) + one audio packet.
        let sps = video_rtp(1, 0x27, &[0x64, 0x00, 0x1f, 0x00]);
        let pps = video_rtp(2, 0x28, &[0xce, 0x88]);
        let idr = video_rtp(3, 0x65, &[0x00, 0x00, 0x00, 0x00, 0x00]);
        let audio = audio_rtp(1);
        let port = spawn_rtsp_mock(vec![
            (0, sps),
            (0, pps),
            (0, idr),
            (2, audio),
        ])
        .await;

        let vs = VideoServer::new();
        vs.start("127.0.0.1", port, 0, "u", "p", &origin())
            .await
            .unwrap();

        // Armed + has video once the pipeline is up.
        assert!(vs.has_video());
        assert!(vs.armed_origin().is_some());
        assert!(vs.subscribe_audio().is_some());

        // The video broadcast must deliver FLV tags (sequence header and/or
        // video tag) derived from the streamed NALs.
        let mut rx = vs.subscribe_video().expect("video broadcast armed");
        let tag = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("pipeline did not emit a video tag within 3s")
            .expect("video channel closed before a tag");
        assert!(!tag.is_empty(), "broadcast a non-empty FLV tag");

        vs.stop().await;
        // Disarmed after stop.
        assert!(!vs.has_video());
        assert!(vs.armed_origin().is_none());
        assert!(vs.subscribe_video().is_none());
    }
}
