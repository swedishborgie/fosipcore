/// RTSP client: connects to the camera's RTSP server, completes the
/// handshake (OPTIONS → DESCRIBE → SETUP → PLAY), and reads RTP
/// packets received over TCP interleaved channels.
///
/// ## Protocol
///
/// RTSP is a text-based request/response protocol similar to HTTP.
/// Many cameras require Digest authentication (RFC 2617) on DESCRIBE
/// and SETUP. This client handles the 401 → Digest retry flow.
///
/// After SETUP, RTP data is interleaved on the RTSP TCP socket:
///   `$` + 1-byte channel + 2-byte big-endian length + payload
use anyhow::{bail, Context, Result};
use base64::Engine;
use md5::{Digest, Md5};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// The kind of an SDP media section (`m=` line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// `m=video` — the camera's H.264 track.
    Video,
    /// `m=audio` — the camera's G.711 μ-law track.
    Audio,
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        })
    }
}

/// One media section from the SDP body: an `m=` line plus its
/// section-level `a=control` attribute.
#[derive(Debug, Clone)]
pub struct SdpMedia {
    /// Video or audio.
    pub kind: MediaKind,
    /// First payload type listed on the `m=` line (96 for the camera's
    /// H.264, 0/PCMU for its G.711 track).
    pub payload_type: u8,
    /// `a=control` attribute, if present and not `*`.
    pub control: Option<String>,
}

/// A packet read from the RTSP interleaved channels.
#[derive(Debug)]
pub enum RtpPacket {
    /// Video track (interleaved channels 0-1).
    Video(Vec<u8>),
    /// Audio track (interleaved channels 2-3).
    Audio(Vec<u8>),
}

/// RTSP client connected to a camera.
pub struct RtspClient {
    stream: BufReader<TcpStream>,
    /// RTSP CSeq counter, incremented with each request.
    cseq: u32,
    /// Session ID returned by the SETUP response.
    session: Option<String>,
    /// Camera IP for building RTSP URLs.
    host: String,
    /// Camera RTSP port.
    port: u16,
    /// Stream path (e.g., "videoMain").
    path: String,
    /// Username for Digest auth.
    user: String,
    /// Password for Digest auth.
    pwd: String,
}

impl RtspClient {
    /// Connect to the camera and complete the RTSP handshake.
    ///
    /// Performs the full four-step handshake: OPTIONS → DESCRIBE →
    /// SETUP → PLAY, with Digest authentication retry on 401.
    /// After this returns successfully, call [`read_rtp_packet()`]
    /// to receive RTP data.
    pub async fn connect(host: &str, port: u16, path: &str, user: &str, pwd: &str) -> Result<Self> {
        let addr = format!("{host}:{port}");
        tracing::debug!("RTSP: connecting to {addr}");

        let tcp = TcpStream::connect(&addr)
            .await
            .context("RTSP TCP connect failed")?;

        let stream = BufReader::new(tcp);
        let mut client = Self {
            stream,
            cseq: 1,
            session: None,
            host: host.to_string(),
            port,
            path: path.to_string(),
            user: user.to_string(),
            pwd: pwd.to_string(),
        };

        client.handshake().await?;
        Ok(client)
    }

    /// Complete the RTSP handshake: OPTIONS → DESCRIBE → SETUP → PLAY.
    ///
    /// Every SDP media section is SET UP in order: track 0 gets
    /// interleaved channels 0-1, track 1 gets 2-3, and so on (RTP on
    /// the even channel, RTCP on the odd one).
    async fn handshake(&mut self) -> Result<()> {
        // 1. OPTIONS (usually no auth needed)
        self.options().await?;

        // 2. DESCRIBE (may need Digest auth)
        let media = self.describe().await?;

        // 3. SETUP every track (may need Digest auth with same nonce)
        self.setup_all(&media).await?;

        // 4. PLAY (usually uses the session cookie, no auth needed after SETUP)
        self.play().await?;

        Ok(())
    }

    /// Send OPTIONS request.
    async fn options(&mut self) -> Result<()> {
        let url = self.rtsp_url();
        let request = format!(
            "OPTIONS {url} RTSP/1.0\r\nCSeq: {}\r\n\r\n",
            self.next_cseq()
        );
        tracing::debug!("RTSP: OPTIONS request");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!("RTSP: OPTIONS response code={}", response.status_code);

        if response.status_code == 401 {
            // Some cameras require auth even for OPTIONS — ignore and let
            // the DESCRIBE retry handle it.
            tracing::debug!("RTSP: OPTIONS returned 401 (will authenticate on DESCRIBE)");
        }
        Ok(())
    }

    /// Send DESCRIBE request, retrying with Basic auth then Digest auth on 401.
    ///
    /// Some cameras require Digest auth — no reconnect between requests.
    /// Strategy:
    ///   1. Try unauthenticated → if 200, done.
    ///   2. Same connection: get challenge + send Digest auth → done.
    ///   3. Reconnect and retry once if auth fails.
    async fn describe(&mut self) -> Result<Vec<SdpMedia>> {
        let url = self.rtsp_url();

        // Attempt 1: Unauthenticated (same connection)
        match self.try_describe(&url, None).await {
            Ok(media) => return Ok(media),
            Err(e) => tracing::debug!("RTSP: DESCRIBE no-auth: {e}"),
        }

        // Attempt 2: Digest auth on same connection (challenge + auth back-to-back)
        match self.digest_describe(&url).await {
            Ok(media) => return Ok(media),
            Err(e) => tracing::debug!("RTSP: DESCRIBE Digest: {e}"),
        }

        // Attempt 3: Reconnect, fresh Digest challenge+auth
        self.reconnect().await?;
        self.digest_describe(&url).await
    }

    /// Get a Digest challenge and immediately send the auth'd request
    /// on the SAME connection. LIVE555 ties nonces to the TCP connection.
    async fn digest_describe(&mut self, url: &str) -> Result<Vec<SdpMedia>> {
        // Phase 1: send unauthenticated DESCRIBE to get challenge
        let request = format!(
            "DESCRIBE {url} RTSP/1.0\r\nCSeq: {}\r\nAccept: application/sdp\r\n\r\n",
            self.next_cseq()
        );
        tracing::debug!("RTSP: digest_describe — fetching challenge");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!("RTSP: challenge code={}", response.status_code);

        if response.status_code == 200 {
            let media = parse_sdp_media(&response.body)?;
            tracing::debug!("RTSP: SDP {} media sections (no auth needed)", media.len());
            return Ok(media);
        }

        if response.status_code != 401 {
            bail!("RTSP: expected 401, got {}", response.status_code);
        }

        let www_auth = extract_header(&response.headers, "WWW-Authenticate")
            .ok_or_else(|| anyhow::anyhow!("RTSP: 401 missing WWW-Authenticate"))?;

        // Phase 2: IMMEDIATELY send auth'd request on SAME connection.
        let auth_header = self.digest_auth_header("DESCRIBE", url, www_auth)?;
        let cseq = self.next_cseq();
        let auth_request = format!(
            "DESCRIBE {url} RTSP/1.0\r\n\
             CSeq: {cseq}\r\n\
             Accept: application/sdp\r\n\
             Authorization: {auth_header}\r\n\
             \r\n"
        );
        tracing::debug!("RTSP: digest_describe — sending auth on same connection");
        self.send_request(&auth_request).await?;

        // Read response with timeout on initial status line, then read rest.
        let mut status_line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.stream.read_line(&mut status_line),
        )
        .await
        .map_err(|_| anyhow::anyhow!("RTSP: timeout waiting for auth response"))?
        .context("RTSP: failed to read auth status line")?;

        let status_code = parse_status_code(&status_line)?;

        // Read remaining headers
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            self.stream
                .read_line(&mut line)
                .await
                .context("RTSP: failed to read auth headers")?;
            if line.trim().is_empty() {
                break;
            }
            headers.push_str(&line);
        }

        // Read body if Content-Length present
        let content_length = extract_header(&headers, "Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        tracing::debug!("RTSP: auth response Content-Length={content_length}");

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            // IMPORTANT: read_exact from the BufReader (not get_mut()),
            // because read_line above may have buffered the body bytes.
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.stream.read_exact(&mut body),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!("RTSP: timeout reading auth body ({content_length} bytes)")
            })?
            .context("RTSP: failed to read auth body")?;
        }
        let body_str = String::from_utf8_lossy(&body).to_string();

        tracing::debug!(
            "RTSP: digest auth code={}, body_len={content_length}",
            status_code
        );

        if status_code == 200 {
            tracing::debug!("RTSP: SDP:\n{body_str}");
            let media = parse_sdp_media(&body_str)?;
            tracing::debug!("RTSP: SDP {} media sections", media.len());
            return Ok(media);
        }

        bail!("RTSP: Digest auth rejected ({status_code})")
    }

    /// Send DESCRIBE, optionally with a Digest challenge.
    ///
    /// Returns the parsed media sections on 200, or an error.
    async fn try_describe(&mut self, url: &str, challenge: Option<&str>) -> Result<Vec<SdpMedia>> {
        let cseq = self.next_cseq();
        let mut request =
            format!("DESCRIBE {url} RTSP/1.0\r\nCSeq: {cseq}\r\nAccept: application/sdp\r\n");

        if let Some(ch) = challenge {
            let auth = self.digest_auth_header("DESCRIBE", url, ch)?;
            request.push_str(&format!("Authorization: {auth}\r\n"));
        }
        request.push_str("\r\n");

        let has_auth = challenge.is_some();
        tracing::debug!("RTSP: DESCRIBE request (auth={has_auth})");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!(
            "RTSP: DESCRIBE code={}, body_len={}",
            response.status_code,
            response.body.len()
        );

        match response.status_code {
            200 => {
                tracing::debug!("RTSP: DESCRIBE SDP:\n{}", response.body);
                parse_sdp_media(&response.body)
            }
            401 => bail!("RTSP: got 401"),
            code => bail!("RTSP: unexpected DESCRIBE status {code}"),
        }
    }

    /// Send DESCRIBE with Basic auth (credentials in URL style).
    async fn try_describe_basic(&mut self, url: &str) -> Result<Vec<SdpMedia>> {
        let creds =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.user, self.pwd));
        let cseq = self.next_cseq();
        let request = format!(
            "DESCRIBE {url} RTSP/1.0\r\n\
             CSeq: {cseq}\r\n\
             Accept: application/sdp\r\n\
             Authorization: Basic {creds}\r\n\
             \r\n"
        );
        tracing::debug!("RTSP: DESCRIBE with Basic auth");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!(
            "RTSP: DESCRIBE Basic code={}, body_len={}",
            response.status_code,
            response.body.len()
        );

        if response.status_code == 200 {
            parse_sdp_media(&response.body)
        } else {
            bail!("RTSP: Basic auth rejected ({})", response.status_code)
        }
    }

    /// Send an unauthenticated request and extract the Digest challenge from the 401.
    async fn get_digest_challenge(&mut self, method: &str, url: &str) -> Result<String> {
        let request = format!(
            "{method} {url} RTSP/1.0\r\nCSeq: {}\r\nAccept: application/sdp\r\n\r\n",
            self.next_cseq()
        );
        tracing::debug!("RTSP: fetching Digest challenge ({method})");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!("RTSP: challenge code={}", response.status_code);

        if response.status_code == 200 {
            bail!("RTSP: unexpected 200 — no auth needed?");
        }
        if response.status_code != 401 {
            bail!("RTSP: unexpected challenge status {}", response.status_code);
        }

        extract_header(&response.headers, "WWW-Authenticate")
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("RTSP: 401 missing WWW-Authenticate header"))
    }

    /// SET UP every SDP media section.
    ///
    /// Tracks are SET UP in SDP order on interleaved channel pairs
    /// 0-1, 2-3, … . If any SETUP is rejected with 401, the whole set
    /// is redone on a fresh connection using Digest auth — LIVE555
    /// ties nonces to the TCP connection, so the challenge must be
    /// fetched and consumed on the SAME connection (the old session is
    /// dropped by the reconnect, so all tracks need re-SETUP anyway).
    async fn setup_all(&mut self, media: &[SdpMedia]) -> Result<()> {
        // Phase 1: try unauthenticated.
        if self.setup_all_tracks(media, None).await.is_ok() {
            return Ok(());
        }
        tracing::debug!("RTSP: unauthenticated SETUP rejected — Digest flow");

        // Phase 2: fresh connection, fetch the challenge, then SETUP all
        // tracks with auth on that same connection.
        self.reconnect().await?;
        let challenge = self.fetch_setup_challenge(media).await?;
        self.setup_all_tracks(media, Some(&challenge)).await?;
        Ok(())
    }

    /// SETUP each track. With `challenge = Some(..)`, each track gets a
    /// per-URI Digest Authorization header (HA2 is per-URI, so the
    /// header is computed per track).
    async fn setup_all_tracks(
        &mut self,
        media: &[SdpMedia],
        challenge: Option<&str>,
    ) -> Result<()> {
        for (i, m) in media.iter().enumerate() {
            let interleaved_base = u8::try_from(i * 2).unwrap_or(u8::MAX);
            self.do_setup_track(m, interleaved_base, challenge).await?;
        }
        Ok(())
    }

    /// Send one unauthenticated SETUP to fetch a fresh 401 challenge.
    async fn fetch_setup_challenge(&mut self, media: &[SdpMedia]) -> Result<String> {
        let first = media
            .first()
            .ok_or_else(|| anyhow::anyhow!("RTSP: no media sections to SETUP"))?;
        let track_url = self.track_url(first)?;
        let request = format!(
            "SETUP {track_url} RTSP/1.0\r\n\
             CSeq: {}\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
             \r\n",
            self.next_cseq()
        );
        self.send_request(&request).await?;
        let response = self.read_response().await?;
        tracing::debug!("RTSP: challenge code={}", response.status_code);
        if response.status_code != 401 {
            bail!("RTSP: expected 401 challenge, got {}", response.status_code);
        }
        extract_header(&response.headers, "WWW-Authenticate")
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("RTSP: 401 missing WWW-Authenticate"))
    }

    /// Send one SETUP request for a single track.
    async fn do_setup_track(
        &mut self,
        media: &SdpMedia,
        interleaved_base: u8,
        challenge: Option<&str>,
    ) -> Result<()> {
        let track_url = self.track_url(media)?;
        let auth = challenge
            .map(|ch| self.digest_auth_header("SETUP", &track_url, ch))
            .transpose()?;
        let request = Self::setup_request(
            &track_url,
            self.next_cseq(),
            self.session.as_deref(),
            auth.as_deref(),
            interleaved_base,
        );
        tracing::debug!(
            "RTSP: SETUP {} url={track_url} interleaved={interleaved_base}-{}",
            media.kind,
            interleaved_base + 1
        );
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!("RTSP: SETUP response code={}", response.status_code);
        if response.status_code != 200 {
            bail!(
                "RTSP: SETUP {} failed with {}",
                media.kind,
                response.status_code
            );
        }

        // The first SETUP response allocates the session; later ones
        // echo it. Always take the freshest value.
        if let Some(s) = extract_header(&response.headers, "Session") {
            self.session = Some(s.to_string());
        }
        tracing::debug!("RTSP: session={:?}", self.session);
        Ok(())
    }

    /// Build a SETUP request.
    ///
    /// `session` is `None` for the first SETUP in a session (no
    /// Session header); later tracks echo it. `auth` is the optional
    /// Digest Authorization header value. Absent optional headers
    /// contribute nothing to the header block (no stray lines).
    fn setup_request(
        track_url: &str,
        cseq: u32,
        session: Option<&str>,
        auth: Option<&str>,
        interleaved_base: u8,
    ) -> String {
        // Optional header strings carry their own CRLF (or are empty).
        let session_hdr = session
            .map(|s| format!("Session: {s}\r\n"))
            .unwrap_or_default();
        let auth_hdr = auth
            .map(|h| format!("Authorization: {h}\r\n"))
            .unwrap_or_default();
        format!(
            "SETUP {track_url} RTSP/1.0\r\n\
             CSeq: {cseq}\r\n\
             {session_hdr}{auth_hdr}\
             Transport: RTP/AVP/TCP;unicast;interleaved={interleaved_base}-{}\r\n\
             \r\n",
            interleaved_base + 1
        )
    }

    /// Build the SETUP URL for a media section from its control attribute.
    ///
    /// Absolute `rtsp://` control URLs (which the camera's SDP may carry) are
    /// only accepted if they point at the exact host:port we dialed. SDP is
    /// plaintext, so a MITM/rogue camera could otherwise steer SETUP — and
    /// its Digest auth header — at any internal host:port.
    fn track_url(&self, media: &SdpMedia) -> Result<String> {
        let base = self.rtsp_url();
        let Some(control) = &media.control else {
            return Ok(base);
        };
        Ok(resolve_control_url(&base, control, &self.host, self.port)?)
    }

    /// Send PLAY request to start the RTP stream.
    async fn play(&mut self) -> Result<()> {
        let url = self.rtsp_url();
        let cseq = self.next_cseq();
        let session = self.session.as_deref().unwrap_or("");
        let request = format!(
            "PLAY {url} RTSP/1.0\r\n\
             CSeq: {cseq}\r\n\
             Session: {session}\r\n\
             \r\n"
        );
        tracing::debug!("RTSP: PLAY request");
        self.send_request(&request).await?;

        let response = self.read_response().await?;
        tracing::debug!("RTSP: PLAY response code={}", response.status_code);
        Ok(())
    }

    /// Reconnect the TCP stream (used on 401 to get a fresh connection).
    async fn reconnect(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.host, self.port);
        tracing::debug!("RTSP: reconnecting to {addr}");
        let tcp =
            tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(&addr))
                .await
                .context("RTSP: reconnect timeout")?
                .context("RTSP: reconnect failed")?;
        self.stream = BufReader::new(tcp);
        self.cseq = 1;
        Ok(())
    }

    /// Read the next interleaved RTP packet from the TCP stream.
    ///
    /// Channel 0 → video RTP, channel 1 → video RTCP (consumed,
    /// not surfaced), channel 2 → audio RTP, channel 3 → audio RTCP
    /// (consumed).
    pub async fn read_rtp_packet(&mut self) -> Result<RtpPacket> {
        loop {
            let magic = self.read_u8().await?;
            if magic != b'$' {
                tracing::warn!("RTSP: expected '$', got 0x{magic:02x}, resyncing");
                continue;
            }

            let channel = self.read_u8().await?;
            let len = self.read_be16().await? as usize;

            let mut payload = vec![0u8; len];
            self.stream
                .get_mut()
                .read_exact(&mut payload)
                .await
                .context("RTSP: failed to read RTP payload")?;

            match channel {
                0x00 => return Ok(RtpPacket::Video(payload)),
                0x01 => tracing::trace!("RTSP: consumed video RTCP packet, {len} bytes"),
                0x02 => return Ok(RtpPacket::Audio(payload)),
                0x03 => tracing::trace!("RTSP: consumed audio RTCP packet, {len} bytes"),
                ch => tracing::warn!(
                    "RTSP: unexpected interleaved channel 0x{ch:02x}, discarding {len} bytes"
                ),
            }
        }
    }

    /// Send a TEARDOWN request to gracefully stop the stream.
    #[allow(dead_code)]
    pub async fn teardown(&mut self) -> Result<()> {
        let url = self.rtsp_url();
        let cseq = self.next_cseq();
        let session = self.session.as_deref().unwrap_or("");
        let request = format!(
            "TEARDOWN {url} RTSP/1.0\r\n\
             CSeq: {cseq}\r\n\
             Session: {session}\r\n\
             \r\n"
        );
        self.send_request(&request).await?;
        tracing::debug!("RTSP: TEARDOWN sent");
        Ok(())
    }

    // --- Private helpers ---

    fn rtsp_url(&self) -> String {
        format!("rtsp://{}:{}/{}", self.host, self.port, self.path)
    }

    fn next_cseq(&mut self) -> u32 {
        let c = self.cseq;
        self.cseq += 1;
        c
    }

    /// Build a Digest authentication header from a WWW-Authenticate challenge.
    ///
    /// Supports both RFC 2069 (no qop) and RFC 2617 (qop=auth).
    /// Handles `algorithm=MD5` and `algorithm=MD5-sess` if present.
    fn digest_auth_header(&self, method: &str, uri: &str, www_auth: &str) -> Result<String> {
        tracing::debug!("RTSP: WWW-Authenticate: {www_auth}");

        let realm = extract_auth_param(www_auth, "realm")
            .ok_or_else(|| anyhow::anyhow!("RTSP: Digest challenge missing realm"))?;
        let nonce = extract_auth_param(www_auth, "nonce")
            .ok_or_else(|| anyhow::anyhow!("RTSP: Digest challenge missing nonce"))?;
        let opaque = extract_auth_param(www_auth, "opaque");
        let qop = extract_auth_param(www_auth, "qop");
        let algorithm = extract_auth_param(www_auth, "algorithm");

        tracing::debug!(
            "RTSP: Digest params - realm={realm}, nonce={nonce}, qop={qop:?}, algorithm={algorithm:?}, opaque={opaque:?}"
        );

        // Generate client nonce (random hex string)
        let cnonce = generate_cnonce();
        // Nonce count — always 1 for fresh connections
        let nc = "00000001";

        // HA1 = MD5(username:realm:password)
        let ha1 = md5_hex(format!("{}:{}:{}", self.user, realm, self.pwd));

        // HA2 = MD5(method:uri)
        let ha2 = md5_hex(format!("{method}:{uri}"));

        // Compute response based on qop presence
        let response = if qop == Some("auth") || qop == Some("\"auth\"") {
            // RFC 2617: response = MD5(HA1:nonce:nc:cnonce:qop:HA2)
            let qop_val = "auth";
            tracing::debug!("RTSP: using RFC 2617 qop=auth, cnonce={cnonce}");
            md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop_val}:{ha2}"))
        } else {
            // RFC 2069: response = MD5(HA1:nonce:HA2)
            tracing::debug!("RTSP: using RFC 2069 (no qop)");
            md5_hex(format!("{ha1}:{nonce}:{ha2}"))
        };

        // Build the Authorization header
        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
            self.user, realm, nonce
        );

        if let Some(algo) = algorithm {
            let algo_clean = algo.trim_matches('\"');
            header.push_str(&format!(", algorithm={algo_clean}"));
        }

        if let Some(opaque_val) = opaque {
            let op_clean = opaque_val.trim_matches('\"');
            header.push_str(&format!(", opaque=\"{op_clean}\""));
        }

        if qop.is_some() {
            let qop_val = "auth";
            header.push_str(&format!(", qop={qop_val}, cnonce=\"{cnonce}\", nc={nc}"));
        }

        tracing::debug!("RTSP: Digest response computed (realm={realm}, qop={qop:?})");
        Ok(header)
    }

    async fn send_request(&mut self, request: &str) -> Result<()> {
        self.stream
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .context("RTSP: send failed")?;
        self.stream
            .get_mut()
            .flush()
            .await
            .context("RTSP: flush failed")?;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<RtspResponse> {
        let mut status_line = String::new();
        self.stream
            .read_line(&mut status_line)
            .await
            .context("RTSP: failed to read status line")?;

        let status_code = parse_status_code(&status_line)?;

        let mut headers = String::new();
        loop {
            let mut line = String::new();
            self.stream
                .read_line(&mut line)
                .await
                .context("RTSP: failed to read headers")?;
            if line.trim().is_empty() {
                break;
            }
            headers.push_str(&line);
        }

        let content_length = extract_header(&headers, "Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            // Use BufReader's read_exact (not get_mut()), because
            // read_line above may have buffered body bytes.
            self.stream
                .read_exact(&mut body)
                .await
                .context("RTSP: failed to read body")?;
        }
        let body_str = String::from_utf8_lossy(&body).to_string();

        tracing::trace!(
            "RTSP: response status={status_code} headers_len={} body_len={content_length}",
            headers.len()
        );

        Ok(RtspResponse {
            status_code,
            headers,
            body: body_str,
        })
    }

    async fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.stream
            .get_mut()
            .read_exact(&mut buf)
            .await
            .context("RTSP: read u8 failed")?;
        Ok(buf[0])
    }

    async fn read_be16(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.stream
            .get_mut()
            .read_exact(&mut buf)
            .await
            .context("RTSP: read be16 failed")?;
        Ok(u16::from_be_bytes(buf))
    }
}

/// A parsed RTSP response.
struct RtspResponse {
    status_code: u16,
    headers: String,
    body: String,
}

/// Generate a random client nonce for Digest auth.
fn generate_cnonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{ts:016x}")
}

/// Compute an MD5 hash and return as lowercase hex.
fn md5_hex(input: impl AsRef<[u8]>) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_ref());
    format!("{:x}", hasher.finalize())
}

/// Parse the status code from an RTSP status line.
fn parse_status_code(line: &str) -> Result<u16> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        bail!("RTSP: invalid status line: {line:?}");
    }
    parts[1]
        .parse()
        .context(format!("RTSP: invalid status code: {}", parts[1]))
}

/// Extract a header value from RTSP response headers.
fn extract_header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    let name_lower = name.to_lowercase();
    for line in headers.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().to_lowercase() == name_lower {
                return Some(value.trim());
            }
        }
    }
    None
}

/// Extract a parameter value from a WWW-Authenticate / Digest challenge string.
///
/// Parses quoted or unquoted values: `realm="IPCAM"` or `nonce=abc123`.
/// Handles the leading `Digest` scheme prefix.
fn extract_auth_param<'a>(auth: &'a str, param: &str) -> Option<&'a str> {
    let search = format!("{param}=");
    let search_lower = search.to_lowercase();

    // Find the parameter in the string (case-insensitive)
    let lower = auth.to_lowercase();
    let pos = lower.find(&search_lower)?;
    let after_eq = &auth[pos + search.len()..];

    // Check if it's quoted
    let after_eq = after_eq.trim();
    if after_eq.starts_with('"') {
        // Quoted value: find closing quote
        let inner = &after_eq[1..];
        let end = inner.find('"')?;
        Some(&inner[..end])
    } else {
        // Unquoted: take until comma, space, or end
        let end = after_eq
            .find([',', ' ', '\r', '\n'])
            .unwrap_or(after_eq.len());
        Some(after_eq[..end].trim())
    }
}

/// Resolve an SDP `a=control:` value against the dialed camera.
///
/// - absent/`*`/empty → the base URL
/// - relative (`track1`, `/track1`) → appended to the base URL
/// - absolute `rtsp://…` → **must** point at the same host:port that
///   fosipcore dialed; anything else is rejected (SDP is plaintext — a
///   hostile camera/MITM must not steer SETUP at other hosts).
fn resolve_control_url(base: &str, control: &str, expected_host: &str, expected_port: u16) -> Result<String> {
    let control = control.trim();
    if control.is_empty() || control == "*" {
        return Ok(base.to_string());
    }
    if let Some(rest) = control.strip_prefix("rtsp://") {
        let authority = rest.split('/').next().unwrap_or("");
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), p.parse::<u16>().unwrap_or(0)),
            None => (authority.to_ascii_lowercase(), 554),
        };
        if host != expected_host.trim().to_ascii_lowercase() || port != expected_port {
            bail!(
                "RTSP: SDP a=control points at {authority}, expected {expected_host}:{expected_port} — refusing"
            );
        }
        return Ok(control.to_string());
    }
    let sep = if control.starts_with('/') || base.ends_with('/') {
        ""
    } else {
        "/"
    };
    Ok(format!("{base}{sep}{control}"))
}

/// Parse the media sections out of an SDP body.
///
/// Returns one [`SdpMedia`] per `m=video` / `m=audio` line, in
/// document order, each carrying its section-level `a=control`
/// attribute (if present and not `*`). Session-level attributes
/// (before the first `m=` line, e.g. `a=control:*`) are ignored.
fn parse_sdp_media(sdp: &str) -> Result<Vec<SdpMedia>> {
    let mut media: Vec<SdpMedia> = Vec::new();
    for line in sdp.lines() {
        let trimmed = line.trim();
        if let Some(m) = trimmed.strip_prefix("m=") {
            // m=<media> <port> <proto> <fmt> [<fmt> ...]
            let mut fields = m.split_whitespace();
            let name = fields.next().unwrap_or("");
            let kind = match name {
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                _ => continue,
            };
            // Field index 3 (0-based: media=0, port=1, proto=2, fmt=3):
            // the first payload type.
            let payload_type = fields
                .nth(2)
                .and_then(|f| f.parse::<u8>().ok())
                .unwrap_or(0);
            media.push(SdpMedia {
                kind,
                payload_type,
                control: None,
            });
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("a=control:") {
            let c = value.trim();
            if c.is_empty() || c == "*" {
                continue;
            }
            if let Some(last) = media.last_mut() {
                last.control = Some(c.to_string());
            }
        }
    }
    if media.is_empty() {
        bail!("RTSP: no media sections found in SDP");
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_code_ok() {
        assert_eq!(parse_status_code("RTSP/1.0 200 OK\r\n").unwrap(), 200);
    }

    #[test]
    fn test_parse_status_code_unauthorized() {
        assert_eq!(
            parse_status_code("RTSP/1.0 401 Unauthorized\r\n").unwrap(),
            401
        );
    }

    #[test]
    fn test_parse_status_code_invalid() {
        assert!(parse_status_code("garbage").is_err());
    }

    #[test]
    fn test_extract_header() {
        let headers = "CSeq: 1\r\nSession: 12345\r\nTransport: RTP/AVP/TCP\r\n";
        assert_eq!(extract_header(headers, "Session"), Some("12345"));
        assert_eq!(extract_header(headers, "CSeq"), Some("1"));
        assert_eq!(extract_header(headers, "Missing"), None);
    }

    #[test]
    fn test_extract_header_case_insensitive() {
        let headers = "session: 54321\r\n";
        assert_eq!(extract_header(headers, "Session"), Some("54321"));
    }

    #[test]
    fn test_extract_auth_param_quoted() {
        let auth = r#"Digest realm="IPCAM", nonce="abc123", opaque="xyz""#;
        assert_eq!(extract_auth_param(auth, "realm"), Some("IPCAM"));
        assert_eq!(extract_auth_param(auth, "nonce"), Some("abc123"));
        assert_eq!(extract_auth_param(auth, "opaque"), Some("xyz"));
    }

    #[test]
    fn test_extract_auth_param_unquoted() {
        let auth = r#"Digest realm=IPCAM, nonce=abc123"#;
        assert_eq!(extract_auth_param(auth, "realm"), Some("IPCAM"));
        assert_eq!(extract_auth_param(auth, "nonce"), Some("abc123"));
    }

    #[test]
    fn test_extract_auth_param_missing() {
        let auth = r#"Digest realm="IPCAM""#;
        assert_eq!(extract_auth_param(auth, "nonce"), None);
    }

    #[test]
    fn test_md5_hex() {
        assert_eq!(md5_hex("hello"), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn test_setup_request_first_track_no_auth() {
        let req = RtspClient::setup_request(
            "rtsp://192.168.1.100:554/live/trackID=1",
            3,
            None, // first SETUP: no Session header
            None, // no Digest auth
            0,
        );
        let expected = "SETUP rtsp://192.168.1.100:554/live/trackID=1 RTSP/1.0\r\n".to_string()
            + "CSeq: 3\r\n"
            + "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n"
            + "\r\n";
        assert_eq!(req, expected);
    }

    #[test]
    fn test_setup_request_second_track_with_session_and_auth() {
        let req = RtspClient::setup_request(
            "rtsp://192.168.1.100:554/live/trackID=2",
            4,
            Some("abc123;timeout=60"),
            Some("Digest username=\"admin\", realm=\"x\", response=\"cafe\""),
            2,
        );
        let expected = "SETUP rtsp://192.168.1.100:554/live/trackID=2 RTSP/1.0\r\n".to_string()
            + "CSeq: 4\r\n"
            + "Session: abc123;timeout=60\r\n"
            + "Authorization: Digest username=\"admin\", realm=\"x\", response=\"cafe\"\r\n"
            + "Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n"
            + "\r\n";
        assert_eq!(req, expected);
    }

    #[test]
    fn test_setup_request_session_but_no_auth() {
        // Audio track with a session but plain-text auth (no Digest):
        // exactly one optional header present, no stray blank lines.
        let req = RtspClient::setup_request("rtsp://h/live/track2", 4, Some("s1"), None, 2);
        let expected = "SETUP rtsp://h/live/track2 RTSP/1.0\r\n".to_string()
            + "CSeq: 4\r\n"
            + "Session: s1\r\n"
            + "Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n"
            + "\r\n";
        assert_eq!(req, expected);
        // No double CRLF anywhere except the final body separator.
        assert_eq!(req.matches("\r\n\r\n").count(), 1);
    }

    #[test]
    fn test_parse_sdp_media_video_only() {
        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=1\r\n";
        let media = parse_sdp_media(sdp).expect("parse");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].kind, MediaKind::Video);
        assert_eq!(media[0].payload_type, 96);
        assert_eq!(media[0].control.as_deref(), Some("trackID=1"));
    }

    #[test]
    fn test_parse_sdp_media_real_camera() {
        // Real camera SDP: session-level `*`, video track1 (PT 96),
        // audio track2 (PT 0 = PCMU).
        let sdp = "v=0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=control:track1\r\nm=audio 0 RTP/AVP 0\r\na=control:track2\r\n";
        let media = parse_sdp_media(sdp).expect("parse");
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, MediaKind::Video);
        assert_eq!(media[0].payload_type, 96);
        assert_eq!(media[0].control.as_deref(), Some("track1"));
        assert_eq!(media[1].kind, MediaKind::Audio);
        assert_eq!(media[1].payload_type, 0);
        assert_eq!(media[1].control.as_deref(), Some("track2"));
    }

    #[test]
    fn test_parse_sdp_media_no_control() {
        // No a=control anywhere: controls stay None (SETUP falls back
        // to the base URL).
        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\nm=audio 0 RTP/AVP 0\r\n";
        let media = parse_sdp_media(sdp).expect("parse");
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].control, None);
        assert_eq!(media[1].control, None);
    }

    #[test]
    fn test_parse_sdp_media_ignores_unknown_media() {
        let sdp = "v=0\r\nm=application 0 RTP/AVP 96\r\nm=video 0 RTP/AVP 96\r\n";
        let media = parse_sdp_media(sdp).expect("parse");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].kind, MediaKind::Video);
    }

    #[test]
    fn test_parse_sdp_media_missing() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\n";
        assert!(parse_sdp_media(sdp).is_err());
    }

    // --- a=control URL constraint (Phase 2) ---

    #[test]
    fn control_relative_appends_to_base() {
        let url = resolve_control_url("rtsp://192.168.1.50:88/videoMain", "track1", "192.168.1.50", 88)
            .unwrap();
        assert_eq!(url, "rtsp://192.168.1.50:88/videoMain/track1");

        let url = resolve_control_url("rtsp://192.168.1.50:88/videoMain", "/track2", "192.168.1.50", 88)
            .unwrap();
        assert_eq!(url, "rtsp://192.168.1.50:88/videoMain/track2");
    }

    #[test]
    fn control_wildcard_or_empty_uses_base() {
        assert_eq!(
            resolve_control_url("rtsp://h:88/v", "*", "h", 88).unwrap(),
            "rtsp://h:88/v"
        );
        assert_eq!(
            resolve_control_url("rtsp://h:88/v", "", "h", 88).unwrap(),
            "rtsp://h:88/v"
        );
    }

    #[test]
    fn control_absolute_same_host_port_ok() {
        let url = resolve_control_url(
            "rtsp://192.168.1.50:88/videoMain",
            "rtsp://192.168.1.50:88/videoMain/track1",
            "192.168.1.50",
            88,
        )
        .unwrap();
        assert_eq!(url, "rtsp://192.168.1.50:88/videoMain/track1");
    }

    // --- Tier 2: in-process mock RTSP server driving the real client ---
    //
    // Binds 127.0.0.1:0 (ephemeral, parallel-safe) and speaks just enough
    // RTSP over TCP to run the full handshake + interleaved-RTP read loop.
    // The server accepts multiple sequential connections so the client's
    // 401 → reconnect flow works.

    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// A request the mock observed from the client.
    struct Req {
        method: String,
        url: String,
        cseq: Option<u32>,
        session: Option<String>,
        auth: Option<String>,
        transport: Option<String>,
    }

    /// Digest challenge config (None disables auth on the mock).
    #[derive(Clone)]
    struct Auth {
        realm: String,
        nonce: String,
        qop: bool,
        #[allow(dead_code)]
        user: String,
        #[allow(dead_code)]
        pwd: String,
    }

    #[derive(Clone)]
    struct MockOpts {
        auth: Option<Auth>,
        sdp: String,
        session_id: String,
        /// Interleaved packets to push after PLAY: (channel, payload).
        rtp: Vec<(u8, Vec<u8>)>,
        /// Close the connection right after the PLAY response (no RTP).
        close_after_play: bool,
        /// Respond 401 to an auth-bearing DESCRIBE (Digest rejected).
        reject_auth: bool,
    }

    fn challenge(a: &Auth) -> String {
        let qop = if a.qop { r#", qop="auth""# } else { "" };
        format!(
            "RTSP/1.0 401 Unauthorized\r\n\
             WWW-Authenticate: Digest realm=\"{}\", nonce=\"{}\"{qop}\r\n\r\n",
            a.realm, a.nonce
        )
    }

    fn sdp_200(sdp: &str) -> String {
        format!(
            "RTSP/1.0 200 OK\r\nContent-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n{}",
            sdp.len(),
            sdp
        )
    }

    fn setup_200(session: &str) -> String {
        format!("RTSP/1.0 200 OK\r\nSession: {session}\r\n\r\n")
    }

    /// Spin up the mock on an ephemeral port. Returns (port, captured reqs).
    async fn spawn_mock(opts: MockOpts) -> (u16, Arc<Mutex<Vec<Req>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let port = listener.local_addr().unwrap().port();
        let reqs: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(Vec::new()));
        let reqs_c = reqs.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let reqs = reqs_c.clone();
                let opts = opts.clone();
                tokio::spawn(mock_conn(sock, opts, reqs));
            }
        });
        (port, reqs)
    }

    async fn mock_conn(
        sock: tokio::net::TcpStream,
        opts: MockOpts,
        reqs: Arc<Mutex<Vec<Req>>>,
    ) {
        let (r, mut w) = sock.into_split();
        let mut br = tokio::io::BufReader::new(r);
        loop {
            let mut line = String::new();
            if br.read_line(&mut line).await.unwrap_or(0) == 0 {
                return; // client closed
            }
            let mut headers = String::new();
            loop {
                let mut h = String::new();
                if br.read_line(&mut h).await.unwrap_or(0) == 0 {
                    return;
                }
                if h.trim().is_empty() {
                    break;
                }
                headers.push_str(&h);
            }
            let mut it = line.split_whitespace();
            let method = it.next().unwrap_or("").to_string();
            let url = it.next().unwrap_or("").to_string();
            let cseq = extract_header(&headers, "CSeq").and_then(|v| v.parse().ok());
            let session = extract_header(&headers, "Session").map(str::to_string);
            let auth = extract_header(&headers, "Authorization").map(str::to_string);
            let transport = extract_header(&headers, "Transport").map(str::to_string);
            reqs.lock().unwrap().push(Req {
                method: method.clone(),
                url: url.clone(),
                cseq,
                session: session.clone(),
                auth: auth.clone(),
                transport: transport.clone(),
            });

            // Decide the response.
            let needs_auth = opts.auth.is_some() && auth.is_none();
            if method == "PLAY" {
                // PLAY gets a 200, then any interleaved RTP is pushed in a
                // separate write after a short delay (so the client's
                // BufReader hasn't read ahead past the response). We do NOT
                // close here — a later TEARDOWN must still be readable.
                w.write_all(setup_200(&opts.session_id).as_bytes()).await.ok();
                w.flush().await.ok();
                if opts.close_after_play {
                    return;
                }
                if !opts.rtp.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    for (ch, payload) in &opts.rtp {
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
                continue;
            }
            let resp = match method.as_str() {
                "OPTIONS" | "TEARDOWN" => "RTSP/1.0 200 OK\r\n\r\n".to_string(),
                "DESCRIBE" => {
                    if needs_auth {
                        challenge(&opts.auth.as_ref().unwrap())
                    } else if opts.auth.is_some() && opts.reject_auth {
                        "RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"r\", nonce=\"n\"\r\n\r\n".to_string()
                    } else {
                        sdp_200(&opts.sdp)
                    }
                }
                "SETUP" => {
                    if needs_auth {
                        challenge(&opts.auth.as_ref().unwrap())
                    } else {
                        setup_200(&opts.session_id)
                    }
                }
                _ => "RTSP/1.0 500 Error\r\n\r\n".to_string(),
            };
            w.write_all(resp.as_bytes()).await.ok();
            w.flush().await.ok();
        }
    }

    fn video_only_sdp() -> String {
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 RTP/AVP 96\r\n"
            .to_string()
    }

    fn av_sdp() -> String {
        "v=0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=control:track1\r\nm=audio 0 RTP/AVP 0\r\na=control:track2\r\n".to_string()
    }

    /// Compute the RFC 2617 (qop=auth) Digest response the client should send.
    fn expected_response_qop(user: &str, realm: &str, pwd: &str, method: &str, uri: &str, nonce: &str, cnonce: &str) -> String {
        let ha1 = md5_hex(format!("{user}:{realm}:{pwd}"));
        let ha2 = md5_hex(format!("{method}:{uri}"));
        md5_hex(format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"))
    }

    /// Compute the RFC 2069 (no qop) Digest response.
    fn expected_response_plain(user: &str, realm: &str, pwd: &str, method: &str, uri: &str, nonce: &str) -> String {
        let ha1 = md5_hex(format!("{user}:{realm}:{pwd}"));
        let ha2 = md5_hex(format!("{method}:{uri}"));
        md5_hex(format!("{ha1}:{nonce}:{ha2}"))
    }

    #[tokio::test]
    async fn full_handshake_no_auth() {
        let (port, reqs) = spawn_mock(MockOpts {
            auth: None,
            sdp: video_only_sdp(),
            session_id: "abc123;timeout=60".into(),
            rtp: vec![(0, vec![0x80, 0x01, 0x02, 0x03])],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let mut client =
            RtspClient::connect("127.0.0.1", port, "videoMain", "admin", "secret")
                .await
                .expect("handshake");

        // Method sequence + CSeq monotonically increasing from 1.
        let r = reqs.lock().unwrap();
        let methods: Vec<&str> = r.iter().map(|q| q.method.as_str()).collect();
        assert_eq!(methods, vec!["OPTIONS", "DESCRIBE", "SETUP", "PLAY"]);
        assert_eq!(r[0].cseq, Some(1));
        assert_eq!(r[1].cseq, Some(2));
        assert_eq!(r[2].cseq, Some(3));
        assert_eq!(r[3].cseq, Some(4));
        // SETUP carried interleaved=0-1 and PLAY echoed the Session.
        assert_eq!(r[2].transport.as_deref(), Some("RTP/AVP/TCP;unicast;interleaved=0-1"));
        assert_eq!(r[3].session.as_deref(), Some("abc123;timeout=60"));
        drop(r);

        // RTP flows after PLAY: a video packet on channel 0.
        let pkt = tokio::time::timeout(std::time::Duration::from_secs(3), client.read_rtp_packet())
            .await
            .expect("rtp timed out")
            .expect("rtp ok");
        match pkt {
            RtpPacket::Video(p) => assert_eq!(p, vec![0x80, 0x01, 0x02, 0x03]),
            other => panic!("expected video, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn digest_auth_qop_challenge() {
        let auth = Auth {
            realm: "IPCAM".into(),
            nonce: "n0nc3".into(),
            qop: true,
            user: "admin".into(),
            pwd: "secret".into(),
        };
        let (port, reqs) = spawn_mock(MockOpts {
            auth: Some(auth.clone()),
            sdp: video_only_sdp(),
            session_id: "sess1".into(),
            rtp: vec![],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let client =
            RtspClient::connect("127.0.0.1", port, "videoMain", "admin", "secret")
                .await
                .expect("digest handshake");
        drop(client);

        let r = reqs.lock().unwrap();
        let uri = format!("rtsp://127.0.0.1:{port}/videoMain");
        // The auth'd DESCRIBE is the one whose Authorization carries qop.
        let describe_auth = r
            .iter()
            .find(|q| q.method == "DESCRIBE" && q.auth.is_some())
            .expect("auth'd DESCRIBE");
        let auth_hdr = describe_auth.auth.as_ref().unwrap();
        let cnonce = extract_auth_param(auth_hdr, "cnonce").expect("cnonce");
        let got = extract_auth_param(auth_hdr, "response").expect("response");
        let want = expected_response_qop(
            "admin", "IPCAM", "secret", "DESCRIBE", &uri, "n0nc3", cnonce,
        );
        assert_eq!(got, want, "Digest response must match");
        // qop + nc present in the header.
        assert!(auth_hdr.contains("qop=auth"));
        assert!(auth_hdr.contains("nc=00000001"));
    }

    #[tokio::test]
    async fn digest_auth_no_qop_challenge() {
        let auth = Auth {
            realm: "CAM".into(),
            nonce: "n0nc3".into(),
            qop: false,
            user: "admin".into(),
            pwd: "pw".into(),
        };
        let (port, reqs) = spawn_mock(MockOpts {
            auth: Some(auth),
            sdp: video_only_sdp(),
            session_id: "sess1".into(),
            rtp: vec![],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let client =
            RtspClient::connect("127.0.0.1", port, "live", "admin", "pw")
                .await
                .expect("no-qop digest handshake");
        drop(client);

        let r = reqs.lock().unwrap();
        let uri = format!("rtsp://127.0.0.1:{port}/live");
        let describe_auth = r
            .iter()
            .find(|q| q.method == "DESCRIBE" && q.auth.is_some())
            .expect("auth'd DESCRIBE");
        let auth_hdr = describe_auth.auth.as_ref().unwrap();
        let got = extract_auth_param(auth_hdr, "response").expect("response");
        let want = expected_response_plain("admin", "CAM", "pw", "DESCRIBE", &uri, "n0nc3");
        assert_eq!(got, want);
        // No qop/cnonce in the header for the RFC 2069 path.
        assert!(!auth_hdr.contains("qop="));
        assert!(!auth_hdr.contains("cnonce="));
    }

    #[tokio::test]
    async fn audio_track_setup_two_setups() {
        let (port, reqs) = spawn_mock(MockOpts {
            auth: None,
            sdp: av_sdp(),
            session_id: "avsess".into(),
            rtp: vec![],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let client =
            RtspClient::connect("127.0.0.1", port, "stream", "admin", "pw")
                .await
                .expect("a/v handshake");
        drop(client);

        let r = reqs.lock().unwrap();
        let setups: Vec<&Req> = r.iter().filter(|q| q.method == "SETUP").collect();
        assert_eq!(setups.len(), 2, "one SETUP per media section");
        // Distinct interleaved channel pairs and distinct track control URLs.
        assert_eq!(setups[0].transport.as_deref(), Some("RTP/AVP/TCP;unicast;interleaved=0-1"));
        assert_eq!(setups[1].transport.as_deref(), Some("RTP/AVP/TCP;unicast;interleaved=2-3"));
        let base = format!("rtsp://127.0.0.1:{port}/stream");
        assert_eq!(setups[0].url, format!("{base}/track1"));
        assert_eq!(setups[1].url, format!("{base}/track2"));
        // The second SETUP echoes the Session from the first.
        assert_eq!(setups[1].session.as_deref(), Some("avsess"));
        // PLAY carries the combined session.
        let play = r.iter().find(|q| q.method == "PLAY").expect("PLAY");
        assert_eq!(play.session.as_deref(), Some("avsess"));
    }

    #[tokio::test]
    async fn rtp_read_dispatches_channels() {
        // channel 1 (video RTCP) consumed, 0 (video) returned, 3 (audio RTCP)
        // consumed, 2 (audio) returned, 5 (unknown) discarded, 0 (video) again.
        let (port, _reqs) = spawn_mock(MockOpts {
            auth: None,
            sdp: video_only_sdp(),
            session_id: "s".into(),
            rtp: vec![
                (1, vec![0xAA]),      // video RTCP — consumed
                (0, vec![0x81, 0x01]), // video — returned
                (3, vec![0xBB]),      // audio RTCP — consumed
                (2, vec![0x7F, 0x7F]), // audio — returned
                (5, vec![0xCC]),      // unknown — discarded
                (0, vec![0x82, 0x02]), // video — returned
            ],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let mut client =
            RtspClient::connect("127.0.0.1", port, "v", "u", "p")
                .await
                .expect("handshake");

        macro_rules! read_pkt {
            () => {
                tokio::time::timeout(std::time::Duration::from_secs(3), client.read_rtp_packet())
                    .await
                    .expect("timed out")
                    .expect("read")
            };
        }
        match read_pkt!() {
            RtpPacket::Video(p) => assert_eq!(p, vec![0x81, 0x01]),
            o => panic!("expected video, {o:?}"),
        }
        match read_pkt!() {
            RtpPacket::Audio(p) => assert_eq!(p, vec![0x7F, 0x7F]),
            o => panic!("expected audio, {o:?}"),
        }
        match read_pkt!() {
            RtpPacket::Video(p) => assert_eq!(p, vec![0x82, 0x02]),
            o => panic!("expected video, {o:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_close_surfaces_error() {
        // Server closes right after PLAY with no RTP: read_rtp_packet must
        // return an error (not hang) on EOF.
        let (port, _reqs) = spawn_mock(MockOpts {
            auth: None,
            sdp: video_only_sdp(),
            session_id: "s".into(),
            rtp: vec![],
            close_after_play: true,
            reject_auth: false,
        })
        .await;

        let mut client =
            RtspClient::connect("127.0.0.1", port, "v", "u", "p")
                .await
                .expect("handshake");

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), client.read_rtp_packet())
            .await
            .expect("should not hang");
        assert!(result.is_err(), "EOF after close must be an error, got {result:?}");
    }

    #[tokio::test]
    async fn digest_rejected_is_an_error() {
        // The mock 401s even the auth'd DESCRIBE: the handshake must fail
        // (the client's reconnect+retry also fails) rather than hang.
        let auth = Auth {
            realm: "R".into(),
            nonce: "n".into(),
            qop: false,
            user: "admin".into(),
            pwd: "wrong".into(),
        };
        let (port, _reqs) = spawn_mock(MockOpts {
            auth: Some(auth),
            sdp: video_only_sdp(),
            session_id: "s".into(),
            rtp: vec![],
            close_after_play: false,
            reject_auth: true,
        })
        .await;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            RtspClient::connect("127.0.0.1", port, "v", "admin", "wrong"),
        )
        .await
        .expect("should not hang");
        assert!(result.is_err(), "rejected Digest must fail the handshake");
    }

    #[tokio::test]
    async fn teardown_sends_request() {
        let (port, reqs) = spawn_mock(MockOpts {
            auth: None,
            sdp: video_only_sdp(),
            session_id: "ts".into(),
            rtp: vec![],
            close_after_play: false,
            reject_auth: false,
        })
        .await;

        let mut client =
            RtspClient::connect("127.0.0.1", port, "v", "u", "p")
                .await
                .expect("handshake");
        client.teardown().await.expect("teardown");

        // The mock records the request on its own task; poll until it lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let r = reqs.lock().unwrap();
            if let Some(td) = r.iter().find(|q| q.method == "TEARDOWN") {
                assert_eq!(td.session.as_deref(), Some("ts"));
                drop(r);
                break;
            }
            drop(r);
            if std::time::Instant::now() > deadline {
                panic!("TEARDOWN not recorded by mock");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(client);
    }

    #[test]
    fn control_absolute_foreign_host_rejected() {
        // The passive-abuse case: a hostile SDP steering SETUP (and its
        // Digest auth header) at the cloud metadata service / localhost.
        let err = resolve_control_url(
            "rtsp://192.168.1.50:88/videoMain",
            "rtsp://169.254.169.254/steal",
            "192.168.1.50",
            88,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");

        let err = resolve_control_url(
            "rtsp://192.168.1.50:88/videoMain",
            "rtsp://127.0.0.1:50001/steal",
            "192.168.1.50",
            88,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");

        // Same host, different port → also rejected.
        assert!(
            resolve_control_url(
                "rtsp://192.168.1.50:88/videoMain",
                "rtsp://192.168.1.50:8088/videoMain/track1",
                "192.168.1.50",
                88
            )
            .is_err()
        );
    }
}
