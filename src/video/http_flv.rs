/// HTTP-FLV server: serves playlist manifests and FLV streams to flv.js.
///
/// Endpoints:
///   GET /live/playlist1.json  → Video playlist manifest (JSON)
///   GET /live/playlist2.json  → Audio playlist manifest (JSON)
///   GET /live/stream1.flv     → Video FLV byte stream
///   GET /live/stream2.flv     → Audio FLV byte stream (AAC in FLV)
///
/// One server runs **per session** on that session's allocated live port
/// (see `crate::video::ports::PortPool`), using `hyper` for HTTP/1.1
/// serving. FLV stream endpoints subscribe to the session's `VideoServer`
/// broadcast channel and stream tags to the client as `video/x-flv` with
/// chunked transfer encoding.
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::video::VideoServer;

/// Unified response body type.
type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

/// Apply CORS + no-cache headers to a response builder.
///
/// Echoes the caller's `Origin` rather than `*`: the only legitimate
/// cross-origin fetcher is the camera's own web UI page (origin = the
/// camera), and `*` would let *any* website the user visits read the live
/// stream. `origin_header` is `Some` only for requests that passed the
/// origin gate (see `handle_request`); CLI clients (no `Origin`) get no
/// CORS headers, which they don't need.
fn cors(
    builder: hyper::http::response::Builder,
    origin_header: Option<&str>,
) -> hyper::http::response::Builder {
    let builder = builder.header("Cache-Control", "no-cache");
    match origin_header {
        Some(origin) => builder
            .header("Access-Control-Allow-Origin", origin)
            .header("Access-Control-Allow-Methods", "GET, OPTIONS"),
        None => builder,
    }
}

/// Build a fixed-size response body from bytes.
fn fixed_body(data: impl Into<Bytes>) -> BoxBody {
    #[allow(unreachable_code)]
    Full::new(data.into())
        .map_err(|_: Infallible| -> hyper::Error { unreachable!() })
        .boxed()
}

/// Build a streaming response body from an mpsc receiver of bytes.
fn stream_body(rx: tokio::sync::mpsc::Receiver<Bytes>) -> BoxBody {
    #[allow(unreachable_code)]
    StreamBody::new(ReceiverStream::new(rx).map(|data| Ok(Frame::data(data))))
        .map_err(|_: Infallible| -> hyper::Error { unreachable!() })
        .boxed()
}

/// Start a session's HTTP-FLV server, bound to `port`.
///
/// `port` is the live port this session's login allocated; it is the port
/// the session's `InitInfo` advertised as `livePort`, so the playlist URLs
/// we emit point back at this very listener.
pub async fn run(port: u16, video_server: Arc<VideoServer>) -> Result<(), anyhow::Error> {
    // Loopback only — the camera's web UI fetches FLV from 127.0.0.1
    // (hardcoded playlist URLs), so no legitimate client is remote.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(
        "HTTP-FLV server listening on http://{}",
        listener.local_addr()?
    );

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("HTTP-FLV accept error: {e}");
                continue;
            }
        };

        let server = video_server.clone();
        tokio::spawn(async move {
            tracing::debug!("HTTP-FLV: connection from {peer_addr}");

            let io = TokioIo::new(stream);
            if let Err(e) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req| handle_request(req, server.clone(), port)),
                )
                .await
            {
                if !e.to_string().contains("connection closed") {
                    tracing::error!("HTTP-FLV: connection error: {e}");
                }
            }
        });
    }
}

/// Route an HTTP request to the appropriate handler.
///
/// The live endpoints (playlists + streams) are gated on the request
/// `Origin` matching the camera whose stream is armed — see
/// [`VideoServer::origin_allowed`]. A foreign website's fetch gets 403
/// and no CORS headers; the camera's own UI page (origin = the camera)
/// and CLI clients (no `Origin`) are served.
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    video_server: Arc<VideoServer>,
    port: u16,
) -> Result<Response<BoxBody>, Infallible> {
    let path = req.uri().path().to_string();

    // `Some` only when present AND allowed — used for CORS echoing.
    let origin_header = req.headers().get("origin").and_then(|v| v.to_str().ok());
    let origin_ok = video_server.origin_allowed(origin_header);

    match (req.method(), path.as_str()) {
        (&Method::GET, "/live/playlist1.json") => {
            if !origin_ok {
                return Ok(forbidden_response());
            }
            Ok(serve_playlist(port, "stream1.flv", origin_header))
        }
        (&Method::GET, "/live/playlist2.json") => {
            if !origin_ok {
                return Ok(forbidden_response());
            }
            Ok(serve_playlist(port, "stream2.flv", origin_header))
        }
        (&Method::GET, "/live/stream1.flv") => {
            if !origin_ok {
                return Ok(forbidden_response());
            }
            Ok(serve_video_flv_stream(&video_server, origin_header))
        }
        (&Method::GET, "/live/stream2.flv") => {
            if !origin_ok {
                return Ok(forbidden_response());
            }
            Ok(serve_audio_flv_stream(&video_server, origin_header))
        }
        // Health endpoint: no stream data, safe to leave open for monitoring.
        (&Method::GET, "/") => {
            let b = cors(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/plain"),
                None,
            );
            Ok(b.body(fixed_body("fosipcore HTTP-FLV / OK")).unwrap())
        }
        // Preflight: answer neutrally. The actual GET is what's gated.
        (&Method::OPTIONS, _) => {
            let b = cors(Response::builder().status(StatusCode::NO_CONTENT), None);
            Ok(b.body(fixed_body("")).unwrap())
        }
        _ => {
            let b = cors(
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header("Content-Type", "text/plain"),
                None,
            );
            Ok(b.body(fixed_body("Not Found")).unwrap())
        }
    }
}

/// 403 for a request whose `Origin` is not the armed camera's.
fn forbidden_response() -> Response<BoxBody> {
    tracing::warn!("HTTP-FLV: request rejected — origin does not match armed camera");
    let b = Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("Content-Type", "text/plain");
    b.body(fixed_body("Forbidden")).unwrap()
}

/// Serve a playlist JSON manifest for flv.js.
fn serve_playlist(port: u16, stream_file: &str, origin: Option<&str>) -> Response<BoxBody> {
    let json =
        format!(r#"{{"type":"flv","url":"http://127.0.0.1:{port}/live/{stream_file}"}}"#);
    tracing::debug!("HTTP-FLV: serving playlist for {stream_file}");

    let b = cors(
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json"),
        origin,
    );
    b.body(fixed_body(json)).unwrap()
}

/// Serve the video FLV stream (`stream1.flv`).
fn serve_video_flv_stream(video_server: &VideoServer, origin: Option<&str>) -> Response<BoxBody> {
    match video_server.subscribe_video() {
        Some(rx) => flv_stream_response(rx, "video", crate::video::flv_muxer::flv_header(), origin),
        None => no_stream_response("No video stream active"),
    }
}

/// Serve the audio FLV stream (`stream2.flv`) — AAC tags from the
/// G.711 → AAC pipeline, with an audio-only FLV header.
fn serve_audio_flv_stream(video_server: &VideoServer, origin: Option<&str>) -> Response<BoxBody> {
    match video_server.subscribe_audio() {
        Some(rx) => {
            flv_stream_response(rx, "audio", crate::audio::flv::flv_audio_header(), origin)
        }
        None => no_stream_response("No audio stream active"),
    }
}

/// Build a 404 response for a stream with no active pipeline.
fn no_stream_response(msg: &str) -> Response<BoxBody> {
    tracing::warn!("HTTP-FLV: {msg}");
    let b = cors(
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "text/plain"),
        None,
    );
    // Owned String: Bytes from &str would borrow across the response.
    b.body(fixed_body(msg.to_owned())).unwrap()
}

/// Forward a FLV tag broadcast to a client as a chunked HTTP stream.
///
/// Prepends the given FLV header to the first tag so the stream
/// starts with a complete header + tag sequence.
fn flv_stream_response(
    rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
    label: &str,
    header: Vec<u8>,
    origin: Option<&str>,
) -> Response<BoxBody> {
    tracing::info!("HTTP-FLV: client subscribed to {label} stream");
    // Owned copy: the spawned task must be 'static.
    let label = label.to_string();
    let mut header = Some(header);

    let (tx, mpsc_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let fwd_tx = tx.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        let mut first = true;
        loop {
            match rx.recv().await {
                Ok(data) => {
                    let chunk: Bytes = if first {
                        first = false;
                        let mut hdr = header.take().unwrap_or_default();
                        hdr.extend_from_slice(&data);
                        Bytes::from(hdr)
                    } else {
                        Bytes::from(data)
                    };
                    if fwd_tx.send(chunk).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("HTTP-FLV: {label} broadcast lagged by {n} messages, skipping");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        tracing::debug!("HTTP-FLV: {label} stream writer task ending");
    });

    let b = cors(
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "video/x-flv")
            .header("Connection", "close"),
        origin,
    );
    b.body(stream_body(mpsc_rx)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::OriginInfo;
    use futures_util::FutureExt;

    #[test]
    fn playlist_contains_actual_port() {
        let resp = serve_playlist(20000, "stream1.flv", None);
        let body = resp
            .into_body()
            .collect()
            .now_or_never()
            .unwrap()
            .unwrap()
            .to_bytes();
        let json = String::from_utf8(body.to_vec()).unwrap();
        assert!(json.contains("http://127.0.0.1:20000/live/stream1.flv"), "{json}");

        let resp = serve_playlist(61234, "stream2.flv", Some("http://cam.lan:88"));
        let body = resp
            .into_body()
            .collect()
            .now_or_never()
            .unwrap()
            .unwrap()
            .to_bytes();
        let json = String::from_utf8(body.to_vec()).unwrap();
        assert!(json.contains("http://127.0.0.1:61234/live/stream2.flv"), "{json}");
    }

    #[test]
    fn playlist_cors_echoes_origin_not_wildcard() {
        let resp = serve_playlist(20000, "stream1.flv", Some("http://cam.lan:88"));
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "http://cam.lan:88"
        );

        // No Origin (CLI client) → no ACAO header at all.
        let resp = serve_playlist(20000, "stream1.flv", None);
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }

    #[test]
    fn origin_gate_unarmed_allows() {
        // No pipeline started → nothing live to protect; streams 404 anyway.
        let vs = VideoServer::new();
        assert!(vs.origin_allowed(Some("http://evil.example")));
        assert!(vs.origin_allowed(None));
    }

    #[tokio::test]
    async fn origin_gate_armed_only_allows_camera_origin() {
        let vs = VideoServer::new();
        // Arm the origin without a real pipeline: start() pins the origin
        // before spawning; the RTSP dial to 127.0.0.1:9 (discard) fails
        // harmlessly in the background. Loopback is fine here — origin
        // gating is about the *claimed* host, not the dial address.
        vs.start(
            "127.0.0.1",
            9,
            0,
            "u",
            "p",
            &OriginInfo {
                host: "camera.lan".into(),
                port: 88,
            },
        )
        .await
        .unwrap();

        // The camera's own UI page (served by the camera) → allowed.
        assert!(vs.origin_allowed(Some("http://camera.lan:88")));
        assert!(vs.origin_allowed(Some("http://CAMERA.lan:88/live")));
        // CLI client (no Origin) → allowed (README troubleshooting).
        assert!(vs.origin_allowed(None));
        // Any other website → denied (the passive-abuse case).
        assert!(!vs.origin_allowed(Some("http://evil.example")));
        assert!(!vs.origin_allowed(Some("http://camera.lan:80")));
        // Same host:port over https → allowed: an https page can only
        // exist at cam:88 if the camera itself serves it there.
        assert!(vs.origin_allowed(Some("https://camera.lan:88")));
        // Sandboxed iframe (Origin: null) → denied, not treated as CLI.
        assert!(!vs.origin_allowed(Some("null")));

        vs.stop().await;
        // Disarmed again → open (nothing live).
        assert!(vs.origin_allowed(Some("http://evil.example")));
    }

    /// End-to-end gate check against a real listener: one session's server
    /// on one port serves the armed camera's origin and CLI clients (no
    /// Origin), and rejects foreign websites with 403.
    #[tokio::test]
    async fn session_port_gate_403_foreign_200_own_origin() {
        let vs = VideoServer::new();

        // A free port, standing in for the pool's allocation.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe");
        let port = probe.local_addr().expect("probe addr").port();
        drop(probe);

        tokio::spawn(run(port, vs.clone()));
        vs.start(
            "127.0.0.1",
            9,
            0,
            "u",
            "p",
            &OriginInfo {
                host: "camera.lan".into(),
                port: 88,
            },
        )
        .await
        .expect("arm pipeline");

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/live/playlist1.json");

        // Wait for the listener to come up.
        let mut ready = false;
        for _ in 0..50 {
            match client.get(format!("http://127.0.0.1:{port}/")).send().await {
                Ok(_) => {
                    ready = true;
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        assert!(ready, "listener never came up");

        // The camera's own UI page → 200, playlist points at THIS port.
        let resp = client
            .get(&url)
            .header("Origin", "http://camera.lan:88")
            .send()
            .await
            .expect("own-origin request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.expect("body");
        assert!(body.contains(&format!("http://127.0.0.1:{port}/live/stream1.flv")), "{body}");

        // CLI client (no Origin) → 200 (README troubleshooting path).
        let resp = client.get(&url).send().await.expect("cli request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // Any other website → 403, no CORS echo.
        let resp = client
            .get(&url)
            .header("Origin", "http://evil.example")
            .send()
            .await
            .expect("foreign request");
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(
            resp
                .headers()
                .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );

        vs.stop().await;
    }

    // --- Direct unit tests for the stream/no-stream/forbidden builders ---

    #[test]
    fn forbidden_response_is_403() {
        let r = forbidden_response();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn no_active_stream_is_404() {
        let vs = VideoServer::new(); // no pipeline
        let r = serve_video_flv_stream(&vs, None);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = serve_audio_flv_stream(&vs, None);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    /// `flv_stream_response` (and the `stream_body` it wraps) prepends the FLV
    /// header to the first tag and forwards subsequent tags verbatim.
    #[tokio::test]
    async fn flv_stream_response_precedes_header_then_streams_tags() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
        let resp = flv_stream_response(rx, "video", vec![0x46, 0x4C, 0x56], None);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("Content-Type").unwrap(), "video/x-flv");

        tx.send(vec![1, 2, 3]).unwrap();
        tx.send(vec![4, 5]).unwrap();
        drop(tx); // closes the broadcast → forwarder exits → body ends

        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2),
            async { resp.into_body().collect().await.unwrap().to_bytes() })
            .await
            .expect("stream did not end");
        // First chunk = FLV header + tag1; second = tag2.
        assert_eq!(&bytes[..6], b"FLV\x01\x02\x03");
        assert_eq!(&bytes[6..8], b"\x04\x05");
    }

    // --- End-to-end: untested routing arms against a live pipeline ---

    fn rtp_header(seq: u16, ts: u32) -> Vec<u8> {
        let mut h = vec![0u8; 12];
        h[0] = 0x80;
        h[1] = 96;
        h[2..4].copy_from_slice(&seq.to_be_bytes());
        h[4..8].copy_from_slice(&ts.to_be_bytes());
        h[8..12].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        h
    }

    async fn spawn_rtsp_mock() -> u16 {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else { return };
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
                        w.write_all(b"RTSP/1.0 200 OK\r\n\r\n").await.ok();
                    }
                    "DESCRIBE" => {
                        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 RTP/AVP 96\r\n";
                        let resp = format!(
                            "RTSP/1.0 200 OK\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{}",
                            sdp.len(), sdp);
                        w.write_all(resp.as_bytes()).await.ok();
                    }
                    "SETUP" => {
                        w.write_all(b"RTSP/1.0 200 OK\r\nSession: s\r\n\r\n")
                            .await
                            .ok();
                    }
                    "PLAY" => {
                        w.write_all(b"RTSP/1.0 200 OK\r\nSession: s\r\n\r\n")
                            .await
                            .ok();
                        w.flush().await.ok();
                        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                        // SPS, PPS, IDR single-NAL video + one audio packet.
                        let send = |ch: u8, mut pkt: Vec<u8>| {
                            let len = pkt.len();
                            let mut framed = vec![b'$', ch, (len >> 8) as u8, len as u8];
                            framed.append(&mut pkt);
                            framed
                        };
                        for pkt in [
                            {
                                let mut p = rtp_header(1, 90_000);
                                p.extend_from_slice(&[0x27, 0x64, 0x00, 0x1f]);
                                p
                            },
                            {
                                let mut p = rtp_header(2, 90_000);
                                p.extend_from_slice(&[0x28, 0xce, 0x88]);
                                p
                            },
                            {
                                let mut p = rtp_header(3, 90_000);
                                p.extend_from_slice(&[0x65, 0, 0, 0, 0, 0]);
                                p
                            },
                        ] {
                            w.write_all(&send(0, pkt)).await.ok();
                        }
                        let mut apkt = rtp_header(1, 44_100);
                        apkt.extend_from_slice(&[0xFFu8; 160]);
                        w.write_all(&send(2, apkt)).await.ok();
                        w.flush().await.ok();
                        return;
                    }
                    _ => {
                        w.write_all(b"RTSP/1.0 500 Error\r\n\r\n").await.ok();
                    }
                }
                w.flush().await.ok();
            }
        });
        port
    }

    #[tokio::test]
    async fn routing_arms_serve_live_streams_and_errors() {
        let cam_port = spawn_rtsp_mock().await;
        let vs = VideoServer::new();
        vs.start("127.0.0.1", cam_port, 0, "u", "p", &OriginInfo { host: "camera.lan".into(), port: 88 })
            .await
            .unwrap();

        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_port = probe.local_addr().unwrap().port();
        drop(probe);
        tokio::spawn(run(srv_port, vs.clone()));

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{srv_port}");
        // Wait for the listener.
        let mut ready = false;
        for _ in 0..50 {
            if client.get(format!("{base}/")).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ready);

        let own = |u: String| client.get(u).header("Origin", "http://camera.lan:88");

        // playlist2 arm (stream2 manifest).
        let r = own(format!("{base}/live/playlist2.json")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("stream2.flv"));

        // stream1 (video) + stream2 (audio): send() returns after headers;
        // the FLV body streams lazily, so we only assert status/content-type.
        let r = own(format!("{base}/live/stream1.flv")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers().get("Content-Type").unwrap(), "video/x-flv");
        drop(r);
        let r = client.get(format!("{base}/live/stream2.flv")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers().get("Content-Type").unwrap(), "video/x-flv");
        drop(r);

        // OPTIONS preflight → 204.
        let r = client
            .request(reqwest::Method::OPTIONS, format!("{base}/live/stream1.flv"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);

        // Unknown path → 404.
        let r = client.get(format!("{base}/nope")).send().await.unwrap();
        assert_eq!(r.status(), 404);

        vs.stop().await;
        // Pipeline down → stream endpoints 404 (no_stream_response).
        let r = client.get(format!("{base}/live/stream1.flv")).send().await.unwrap();
        assert_eq!(r.status(), 404);
    }
}
