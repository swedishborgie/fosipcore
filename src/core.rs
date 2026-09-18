/// Core WebSocket server.
///
/// Handles the primary communication with the browser: HELLO handshake,
/// authentication, CGI proxy, video, snapshots, and all other commands.
///
/// The camera's address is discovered from the client's login message
/// (the browser knows it because it loaded the page from the camera).
use anyhow::Result;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::protocol::constants::{
    HEARTBEAT_MSG_ID, HELLO_CMD, HELLO_RESPONSE, RESPONSE_REQUEST_AUDIO, RESPONSE_REQUEST_CGI,
    RESPONSE_REQUEST_CLOSE_VIDEO, RESPONSE_REQUEST_LOGIN, RESPONSE_REQUEST_LOGIN_AGAIN,
    RESPONSE_REQUEST_LOGIN_MSG_INIT_INFO, RESPONSE_REQUEST_LOGIN_PRODUCT_INFO,
    RESPONSE_REQUEST_LOGOUT, RESPONSE_REQUEST_OPEN_VIDEO_SUCCESS, RESPONSE_REQUEST_RECORD,
    RESPONSE_REQUEST_SET_WEB_UPGRADE_PROMPT_ENABLE, RESPONSE_REQUEST_SNAP, RESPONSE_REQUEST_TALK,
    WS_REQUEST_AUDIO, WS_REQUEST_CGI, WS_REQUEST_CLOSE_VIDEO, WS_REQUEST_LOGIN,
    WS_REQUEST_LOGIN_AGAIN, WS_REQUEST_LOGOUT, WS_REQUEST_OPEN_VIDEO, WS_REQUEST_RECORD,
    WS_REQUEST_SNAP, WS_REQUEST_TALK, WS_REQUEST_UPGRADE_PROMPT_ENABLE,
};
use crate::net::OriginInfo;
use crate::protocol::messages::{
    AudioCmd, CgiCmd, CgiResponse, InitInfo, LoginCmd, ProductInfo, RecordCmd, SnapResponse,
    TalkCmd, VideoOpenSuccess, VideoPlayCmd, WsMessage,
};
use crate::proxy::cgi;
use crate::video::ports::PortPool;
use crate::video::VideoServer;
use std::net::SocketAddr;

/// Camera connection info, discovered from the client's login message.
///
/// The browser loads its page from the camera, so it already knows the
/// camera's address and sends it to us in the `ip` / `webPort` fields
/// of the login command. We store it per-session after successful login.
///
/// `addr` is the **pinned** resolved address: validated once at login
/// (`crate::net::resolve_camera`) and reused for every subsequent dial.
/// Re-resolving per dial would open a DNS-rebinding window in which later
/// dials — credentials ride in the query string — could be steered at a
/// rogue host.
#[derive(Debug, Clone)]
struct CameraInfo {
    /// Claimed host (hostname or IP literal), for logs and FLV-origin gating.
    ip: String,
    /// Claimed HTTP (CGI) port.
    http_port: u16,
    /// Claimed RTSP port.
    rtsp_port: u16,
    /// Pinned, login-validated dial address (HTTP/CGI port).
    addr: SocketAddr,
}

/// Side-effect produced by message dispatch.
#[derive(Debug, Default)]
struct DispatchAction {
    /// New camera info (set on successful login).
    new_camera: Option<CameraInfo>,
    /// New auth credentials (set on successful login, `None` on logout).
    new_auth: Option<(String, String)>,
    /// New per-session video state (set on first successful login):
    /// allocated live port, the session's `VideoServer`, and the
    /// HTTP-FLV listener task to abort on teardown.
    new_video: Option<(u16, std::sync::Arc<VideoServer>, tokio::task::JoinHandle<()>)>,
    /// Responses to send back to the client (login sends 3: login response, product info, init info).
    responses: Vec<String>,
}

/// Run the core WebSocket server.
pub async fn run(listener: TcpListener, pool: std::sync::Arc<PortPool>) -> Result<()> {
    tracing::info!("Core server listening on {}", listener.local_addr()?);

    loop {
        let (socket, addr) = listener.accept().await?;
        tracing::info!("Core: connection from {}", addr);
        let pool = pool.clone();
        tokio::spawn(handle_connection(socket, pool));
    }
}

/// Per-session mutable state.
///
/// Video state (`video` / `live_port` / `video_listener`) is created on the
/// first successful login and torn down on logout or WS disconnect —
/// closing a tab releases that session's RTSP pull and live port.
struct SessionState {
    camera: Option<CameraInfo>,
    user: Option<String>,
    pwd: Option<String>,
    /// Video stream type (0=main, 1=sub). Read but not yet acted upon (Phase 2).
    stream_type: u32,
    /// The session's own video server (one pipeline slot).
    video: Option<std::sync::Arc<VideoServer>>,
    /// Live port allocated from the pool for this session's HTTP-FLV listener.
    live_port: Option<u16>,
    /// The session's HTTP-FLV listener task, aborted on teardown.
    video_listener: Option<tokio::task::JoinHandle<()>>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            camera: None,
            user: None,
            pwd: None,
            stream_type: 0,
            video: None,
            live_port: None,
            video_listener: None,
        }
    }

    fn logout(&mut self) {
        self.camera = None;
        self.user = None;
        self.pwd = None;
        self.stream_type = 0;
    }

    /// Stop the session's video pipeline, abort its HTTP-FLV listener, and
    /// return its port to the pool. Safe to call when no video state exists
    /// (double-teardown is a no-op — fields are taken, not read).
    ///
    /// `peer` is the connection's address — the log key that ties this
    /// teardown back to the session's "connection from" line (the stock JS
    /// sends `groupId: 0` for every session, so the address is what makes a
    /// tab followable end-to-end).
    async fn teardown_video(&mut self, pool: &std::sync::Arc<PortPool>, peer: &str) {
        let (Some(video), Some(port)) = (self.video.take(), self.live_port.take()) else {
            return;
        };
        video.stop().await;
        if let Some(handle) = self.video_listener.take() {
            handle.abort();
        }
        pool.release(port).await;
        tracing::info!("Session video teardown: pipeline stopped, port {port} released ({peer})");
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_connection(socket: tokio::net::TcpStream, pool: std::sync::Arc<PortPool>) -> Result<()> {
    let peer_addr = match socket.peer_addr() {
        Ok(a) => a.to_string(),
        Err(_) => "?".into(),
    };

    // Accept the WS upgrade and capture the Origin header. The browser sets
    // it from the URL of the page that opened the socket (a page cannot
    // forge another site's origin); the login handler requires it to match
    // the claimed camera.
    let mut origin: Option<OriginInfo> = None;
    let ws_stream = tokio_tungstenite::accept_hdr_async(
        socket,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            if let Some(v) = request.headers().get("Origin") {
                if let Ok(s) = v.to_str() {
                    origin = OriginInfo::parse(s);
                }
            }
            Ok(response)
        },
    )
    .await?;
    if let Some(o) = &origin {
        tracing::debug!("WS origin: {}:{}", o.host, o.port);
    } else {
        tracing::debug!("WS connection without usable Origin header");
    }
    let (mut write, mut read): (
        SplitSink<WebSocketStream<tokio::net::TcpStream>, Message>,
        futures_util::stream::SplitStream<WebSocketStream<tokio::net::TcpStream>>,
    ) = ws_stream.split();

    // Send HELLO_CMD immediately on connect.
    // Binary UTF-8 — getJsonFromMessage wraps in Int8Array, then ab2str reads
    // each byte as a char code. ASCII/UTF-8 maps correctly this way.
    let hello = WsMessage::new(HELLO_CMD);
    let hello_json = serde_json::to_string(&hello)?;
    write
        .send(Message::Binary(hello_json.into_bytes().into()))
        .await?;
    tracing::debug!("Sent HELLO_CMD");

    let mut session = SessionState::new();
    // Loggable session id (the first message's `groupId`) — the key that
    // correlates this core connection with its manager (50000) frames.
    let mut session_group: Option<u64> = None;

    let loop_result = (async {
        while let Some(result) = read.next().await {
            let text: String = match result? {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Invalid UTF-8: {}", e);
                    continue;
                }
            },
            Message::Close(_) => break,
            Message::Ping(p) => {
                let _ = write.send(Message::Pong(p)).await;
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
        };

            let msg: WsMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Parse error: {}", e);
                    continue;
                }
            };

            // Log the session id once (the manager↔core correlation key).
            if session_group.is_none() {
                session_group = Some(msg.group_id);
                tracing::info!("Core: session group={} from {}", msg.group_id, peer_addr);
            }

            tracing::debug!(
                "Inbound msgId={} (group={}, seq={})",
                msg.msg_id,
                msg.group_id,
                msg.sequence
            );

            let mut action = dispatch_message(&msg, &session, origin.as_ref(), &pool).await;

            // Apply side-effects
            session.camera = session.camera.take().or(action.new_camera.take());
            if let Some((port, video, listener)) = action.new_video.take() {
                session.live_port = Some(port);
                session.video = Some(video);
                session.video_listener = Some(listener);
            }
            match action.new_auth {
                Some((u, p)) => {
                    session.user = Some(u);
                    session.pwd = Some(p);
                }
                None if msg.msg_id == WS_REQUEST_LOGOUT => {
                    session.logout();
                    // Logout tears down the video plane: next login
                    // allocates a fresh port.
                    session.teardown_video(&pool, &peer_addr).await;
                }
                None => {}
            }
            // Track stream type for future video handler logic (Phase 2).
            if msg.msg_id == WS_REQUEST_OPEN_VIDEO {
                session.stream_type = extract_stream_type(&msg);
            }

            // Send all responses (login sends 3: login response, product info, init info)
            for json in action.responses {
                tracing::debug!("Outbound response: {}", &json[..json.len().min(200)]);
                // Binary UTF-8 — getJsonFromMessage wraps in Int8Array.
                write
                    .send(Message::Binary(json.into_bytes().into()))
                    .await?;
            }
        }
        Ok(())
    })
    .await;

    // Teardown on every exit path (Close, read error, or the tab simply
    // going away) — this is what releases a closed tab's RTSP session.
    session.teardown_video(&pool, &peer_addr).await;
    tracing::info!(
        "Core: connection closed: {} (group={})",
        peer_addr,
        session_group.unwrap_or(0)
    );
    loop_result
}

/// Route an inbound message to the appropriate handler.
///
/// A single large `match` on protocol msgId. Not decomposed further —
/// each arm is a thin call to its own handler, so this is just a dispatch table.
#[allow(clippy::too_many_lines)]
async fn dispatch_message(
    msg: &WsMessage,
    session: &SessionState,
    origin: Option<&OriginInfo>,
    pool: &std::sync::Arc<PortPool>,
) -> DispatchAction {
    let camera = session.camera.as_ref();
    let user = session.user.as_deref();
    let pwd = session.pwd.as_deref();

    match msg.msg_id {
        HELLO_RESPONSE => {
            tracing::info!("HELLO handshake complete");
            DispatchAction::default()
        }
        HEARTBEAT_MSG_ID => DispatchAction::default(),
        WS_REQUEST_LOGIN | WS_REQUEST_LOGIN_AGAIN => {
            let is_relogin = msg.msg_id == WS_REQUEST_LOGIN_AGAIN;
            match handle_login(msg, is_relogin, origin, session, pool).await {
                Ok(action) => action,
                Err(e) => {
                    tracing::error!("Login error: {}", cgi::redact_creds(&e.to_string()));
                    DispatchAction::default()
                }
            }
        }
        WS_REQUEST_LOGOUT => match handle_logout() {
            Ok(json) => DispatchAction {
                new_auth: None,
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Logout error: {}", e);
                DispatchAction::default()
            }
        },
        WS_REQUEST_CGI => match handle_cgi(msg, user, pwd, camera).await {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("CGI error: {}", cgi::redact_creds(&e.to_string()));
                DispatchAction {
                    responses: vec![error_response(RESPONSE_REQUEST_CGI, &e.to_string())],
                    ..Default::default()
                }
            }
        },
        WS_REQUEST_SNAP => match handle_snapshot(user, pwd, camera).await {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Snapshot error: {}", cgi::redact_creds(&e.to_string()));
                DispatchAction {
                    responses: vec![error_response(RESPONSE_REQUEST_SNAP, &e.to_string())],
                    ..Default::default()
                }
            }
        },
        WS_REQUEST_OPEN_VIDEO => {
            match handle_open_video(msg, user, pwd, camera, session.video.as_ref(), msg.group_id).await {
                Ok(json) => DispatchAction {
                    responses: vec![json],
                    ..Default::default()
                },
                Err(e) => {
                    tracing::error!("Video open error: {}", cgi::redact_creds(&e.to_string()));
                    DispatchAction::default()
                }
            }
        }
        WS_REQUEST_CLOSE_VIDEO => {
            match handle_close_video(session.video.as_ref(), msg.group_id).await {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Video close error: {}", e);
                DispatchAction::default()
            }
        }
        },
        WS_REQUEST_AUDIO => match handle_audio(msg) {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Audio error: {}", e);
                DispatchAction::default()
            }
        },
        WS_REQUEST_TALK => match handle_talk(msg) {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Talk error: {}", e);
                DispatchAction::default()
            }
        },
        WS_REQUEST_RECORD => match handle_record(msg) {
            Ok(json) => DispatchAction {
                responses: vec![json],
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Record error: {}", e);
                DispatchAction::default()
            }
        },
        WS_REQUEST_UPGRADE_PROMPT_ENABLE => {
            // JS sends 20021 and expects 50027/50028 with {enable: 0|1}
            // We return enable=1 (upgrade prompt enabled, default behavior)
            DispatchAction {
                responses: vec![serde_json::to_string(&WsMessage {
                    version: 1,
                    msg_id: RESPONSE_REQUEST_SET_WEB_UPGRADE_PROMPT_ENABLE,
                    group_id: msg.group_id,
                    sequence: msg.sequence,
                    data_len: 0,
                    cmd_object: Some(serde_json::json!({"enable": 0})),
                })
                .unwrap_or_default()],
                ..Default::default()
            }
        }
        _ => {
            tracing::debug!("Unhandled msgId {}", msg.msg_id);
            DispatchAction::default()
        }
    }
}

/// Build a login-failure dispatch action: one `result: -1` response and no
/// camera/auth state, so the session stays unarmed.
fn failed_login_action(msg_id: u32, reason: &str) -> Result<DispatchAction> {
    let fail = CgiResponse {
        result: -1,
        response: format!("<CGI_Result><code>-1</code><error>{reason}</error></CGI_Result>"),
    };
    let msg = WsMessage::with_response(msg_id, fail)?;
    Ok(DispatchAction {
        responses: vec![serde_json::to_string(&msg)?],
        ..Default::default()
    })
}

/// Build an error JSON response for a given msgId.
fn error_response(msg_id: u32, error_msg: &str) -> String {
    let resp = CgiResponse {
        result: -1,
        response: format!("<CGI_Result><code>-1</code><error>{error_msg}</error></CGI_Result>"),
    };
    WsMessage::with_cmd(msg_id, resp)
        .ok()
        .and_then(|m| serde_json::to_string(&m).ok())
        .unwrap_or_else(|| format!(r#"{{"msgid":{msg_id},"result":-1}}"#))
}

/// Handle login, returning a dispatch action with camera info + auth + response.
///
/// On successful login, sends THREE messages to the client:
/// 1. Login response (50001/50026) with CGI auth result
/// 2. Product info (50008) with `getProductAllInfo` XML → `PluginCallBack(502)`
/// 3. Init info (50009) with stream/image/audio params → `PluginCallBack(100)` → sets `gVar.bLogin = true`
///
/// The first successful login of a session also allocates its live port
/// and starts its own `VideoServer` + HTTP-FLV listener; the port is
/// returned in this session's `InitInfo` (`livePort`) — the same field the
/// reference fills from its per-core port. Re-logins reuse the existing
/// port. Pool exhaustion is reported as a login failure (the session
/// stays unarmed).
#[allow(clippy::too_many_lines)]
async fn handle_login(
    msg: &WsMessage,
    is_relogin: bool,
    origin: Option<&OriginInfo>,
    session: &SessionState,
    pool: &std::sync::Arc<PortPool>,
) -> Result<DispatchAction> {
    tracing::debug!("Login cmd_object present: {}", msg.cmd_object.is_some());
    if let Some(ref cmd_obj) = msg.cmd_object {
        // Log the cmd object with the password masked.
        let mut redacted = cmd_obj.clone();
        if let Some(obj) = redacted.as_object_mut() {
            if obj.contains_key("pwd") {
                obj.insert("pwd".into(), serde_json::json!("***"));
            }
        }
        tracing::debug!("Login cmd_object: {redacted}");
    }
    let cmd: LoginCmd = if let Some(c) = msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        c
    } else {
        let debug_info = match &msg.cmd_object {
            Some(v) => format!(
                "present but parse failed: {}",
                cgi::redact_creds(&v.to_string())
            ),
            None => "cmdObject field missing".into(),
        };
        return Err(anyhow::anyhow!("login cmdObject error: {debug_info}"));
    };

    let user = cmd.usr.clone();
    let pwd = cmd.pwd.clone();

    tracing::info!(
        "{}: user={}, camera={}:{}",
        if is_relogin { "Re-login" } else { "Login" },
        user,
        cmd.ip.as_deref().unwrap_or("<missing>"),
        cmd.web_port.unwrap_or(0),
    );

    // Discover camera address from the login message
    let camera_ip = cmd.ip.clone().unwrap_or_default();
    let http_port = cmd.web_port.unwrap_or(88);
    let rtsp_port = cmd.media_port.unwrap_or(88);

    // Resolve + validate ONCE and pin the address for the whole session.
    // Every later CGI/snapshot/RTSP dial uses this pinned address; a DNS
    // flip after login cannot steer them (credentials ride in query strings).
    let response_msg_id = if is_relogin {
        RESPONSE_REQUEST_LOGIN_AGAIN
    } else {
        RESPONSE_REQUEST_LOGIN
    };
    let Ok(pinned) = crate::net::resolve_camera(&camera_ip, http_port).await else {
        // Log without the error chain — it may embed the claim verbatim
        // (fine, no credentials), but keep it to one line.
        tracing::warn!("Login failed: cannot resolve/validate camera {camera_ip}:{http_port}");
        return failed_login_action(response_msg_id, "camera unreachable");
    };

    // Origin check: the page that sent this login must be served by the
    // camera it claims. The stock UI derives both from window.location, so
    // they always agree; a page loaded from anywhere else cannot present
    // the camera's origin. Reject before dialing anything.
    {
        let Some(origin) = origin else {
            tracing::warn!("Login rejected: no Origin header (camera {camera_ip})");
            return failed_login_action(response_msg_id, "no origin");
        };
        if !origin.matches(&camera_ip, http_port) {
            tracing::warn!(
                "Login rejected: origin {}:{} does not match claimed camera {}:{}",
                origin.host,
                origin.port,
                camera_ip,
                http_port
            );
            return failed_login_action(response_msg_id, "origin does not match claimed camera");
        }
    }

    // Try to authenticate with camera.
    // If we can't even reach the camera, refuse to arm the session: report
    // failure and return no camera/auth state. (The camera serves the UI
    // page itself, so a reachable UI normally implies a reachable camera;
    // this guards against a claimed address we cannot verify.)
    let cgi_resp = match cgi::proxy_login(pinned, &user, &pwd).await {
        Ok(r) => r,
        Err(_e) => {
            // Log without the error chain — reqwest errors embed the full
            // request URL (credentials in the query string).
            tracing::warn!("Login failed: cannot reach camera {pinned}");
            return failed_login_action(response_msg_id, "camera unreachable");
        }
    };

    let camera = CameraInfo {
        ip: camera_ip.clone(),
        http_port,
        rtsp_port,
        addr: pinned,
    };

    // Video plane: allocate this session's live port and spin up its own
    // VideoServer + HTTP-FLV listener (the reference spawns one core
    // process per tab with its own media ports; we do one port + one
    // VideoServer per session). A re-login keeps the existing port.
    let (live_port, new_video) = match ensure_session_video(session, pool, msg.group_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("group={}: {e} — refusing login", msg.group_id);
            return failed_login_action(response_msg_id, "too many concurrent video sessions");
        }
    };

    // 1. Login response (50001 or 50026)
    // The JS reads json.result and json.response at the top level, not inside cmdObject.
    // Response messages are flat — no cmdObject wrapper.
    let login_response = WsMessage::with_response(response_msg_id, cgi_resp)?;

    // 2 & 3. Gather product info + init info from camera (only on initial
    // login). The InitInfo carries this session's live port either way —
    // the stock JS stores `livePort` from its own 50009 and builds the
    // video URL from it.
    let (product_xml, init_info) = if is_relogin {
        (None, InitInfo { live_port, ..Default::default() })
    } else {
        cgi::gather_init_info(camera.addr, &user, &pwd, live_port).await
    };

    let mut responses = vec![serde_json::to_string(&login_response)?];

    // 2. Product info (50008) — JS reads json.response at top level
    if let Some(xml) = product_xml {
        let product = WsMessage::with_response(
            RESPONSE_REQUEST_LOGIN_PRODUCT_INFO,
            ProductInfo { response: xml },
        )?;
        responses.push(serde_json::to_string(&product)?);
    }

    // 3. Init info (50009) — JS reads json.rtmpPort, json.recordState, etc. at top level
    let init_msg = WsMessage::with_response(RESPONSE_REQUEST_LOGIN_MSG_INIT_INFO, init_info)?;
    responses.push(serde_json::to_string(&init_msg)?);

    Ok(DispatchAction {
        new_camera: Some(camera),
        new_auth: Some((user, pwd)),
        new_video,
        responses,
    })
}

/// Allocate (or reuse) the session's live video plane.
///
/// Returns `(live_port, new_video)` where `new_video` is `Some` only when a
/// fresh port + `VideoServer` + HTTP-FLV listener were created for this
/// login (re-logins return the existing port with `None`).
///
/// # Errors
///
/// Fails when the port pool is exhausted (too many concurrent sessions).
async fn ensure_session_video(
    session: &SessionState,
    pool: &std::sync::Arc<PortPool>,
    group_id: u64,
) -> Result<(
    u16,
    Option<(u16, std::sync::Arc<VideoServer>, tokio::task::JoinHandle<()>)>,
)> {
    if let Some(p) = session.live_port {
        return Ok((p, None));
    }
    let Some(p) = pool.alloc().await else {
        return Err(anyhow::anyhow!("live port pool exhausted"));
    };

    let video = VideoServer::new();
    let release_pool = pool.clone();
    let listener_video = video.clone();
    let listener = tokio::spawn(async move {
        if let Err(e) = crate::video::http_flv::run(p, listener_video).await {
            // The port was validated at alloc time; a failed re-bind means
            // someone took it in the gap — release it back to the pool.
            tracing::error!("HTTP-FLV listener for port {p} failed: {e}");
            release_pool.release(p).await;
        }
    });

    tracing::info!(
        "group={}: login allocated live port {p} (active video sessions: {})",
        group_id,
        pool.in_use_count().await
    );

    Ok((p, Some((p, video, listener))))
}

fn handle_logout() -> Result<String> {
    let response = WsMessage::new(RESPONSE_REQUEST_LOGOUT);
    Ok(serde_json::to_string(&response)?)
}

async fn handle_cgi(
    msg: &WsMessage,
    user: Option<&str>,
    pwd: Option<&str>,
    camera: Option<&CameraInfo>,
) -> Result<String> {
    let cmd: CgiCmd = match msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(c) => c,
        None => return Err(anyhow::anyhow!("missing or invalid CGI cmdObject")),
    };

    let (Some(user), Some(pwd)) = (user, pwd) else {
        return Err(anyhow::anyhow!("not authenticated"));
    };
    let Some(camera) = camera else {
        return Err(anyhow::anyhow!("no camera configured (login first)"));
    };

    // Detect full CGI URL string vs bare command.
    // The JS sends the full path, possibly URL-encoded (SendCgiCmd2 fallback):
    //   /cgi-bin/CGIProxy.fcgi?usr=...&cmd=... (raw)
    //   %2Fcgi-bin%2FCGIProxy.fcgi%3F... (urlEncoded)
    let cgi_decoded = urlencoding::decode(&cmd.cgi).unwrap_or_default();
    let cgi_resp = if cgi_decoded.starts_with("/cgi-bin/") || cgi_decoded.starts_with("/CGIProxy") {
        let path = cgi_decoded.trim_start_matches('/');
        cgi::proxy_cgi_full_path(camera.addr, path).await?
    } else {
        cgi::proxy_cgi(camera.addr, user, pwd, &cmd.cgi).await?
    };
    let response = WsMessage::with_response(RESPONSE_REQUEST_CGI, cgi_resp)?;
    Ok(serde_json::to_string(&response)?)
}

async fn handle_snapshot(
    user: Option<&str>,
    pwd: Option<&str>,
    camera: Option<&CameraInfo>,
) -> Result<String> {
    let (Some(user), Some(pwd)) = (user, pwd) else {
        return Err(anyhow::anyhow!("not authenticated"));
    };
    let Some(camera) = camera else {
        return Err(anyhow::anyhow!("no camera configured (login first)"));
    };

    match cgi::proxy_snapshot(camera.addr, user, pwd).await {
        Ok(img_data) => {
            let snap_resp = SnapResponse {
                result: 0,
                img: img_data,
            };
            let response = WsMessage::with_response(RESPONSE_REQUEST_SNAP, snap_resp)?;
            Ok(serde_json::to_string(&response)?)
        }
        Err(e) => {
            tracing::warn!("Snapshot failed: {}", cgi::redact_creds(&e.to_string()));
            let snap_resp = SnapResponse {
                result: -1,
                img: String::new(),
            };
            let response = WsMessage::with_response(RESPONSE_REQUEST_SNAP, snap_resp)?;
            Ok(serde_json::to_string(&response)?)
        }
    }
}

async fn handle_open_video(
    msg: &WsMessage,
    user: Option<&str>,
    pwd: Option<&str>,
    camera: Option<&CameraInfo>,
    video: Option<&std::sync::Arc<VideoServer>>,
    group_id: u64,
) -> Result<String> {
    let cmd: VideoPlayCmd = msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let Some(camera) = camera else {
        return Err(anyhow::anyhow!("no camera configured (login first)"));
    };
    let Some(video) = video else {
        return Err(anyhow::anyhow!("no video session (login first)"));
    };

    tracing::info!(
        "Open video: group={}, stream_type={}, camera={}:{}",
        group_id,
        cmd.stream_type,
        camera.ip,
        camera.rtsp_port
    );

    // Dial the PINNED login-validated address (no re-resolve: a DNS flip
    // after login must not steer the RTSP dial at a different host).
    // Spawn the RTSP → FLV pipeline against it, and pin the claimed camera
    // origin for HTTP-FLV endpoint gating (the page fetching FLV is served
    // by the camera, so its Origin is http://<ip>:<webPort>).
    let vs = video.clone();
    let ip = camera.addr.ip().to_string();
    let rtsp_port = camera.rtsp_port;
    let stream_type = cmd.stream_type;
    let spawn_user = user.unwrap_or_default().to_string();
    let spawn_pwd = pwd.unwrap_or_default().to_string();
    // Origin allowed to fetch the FLV streams = the camera's own page
    // origin (host + HTTP port as claimed in the login message).
    let origin = OriginInfo {
        host: camera.ip.trim().to_ascii_lowercase(),
        port: camera.http_port,
    };

    // Clone for the response before moving into the spawn
    let resp_user = spawn_user.clone();
    let resp_pwd = spawn_pwd.clone();

    tokio::spawn(async move {
        if let Err(e) = vs
            .start(
                &ip,
                rtsp_port,
                stream_type,
                &spawn_user,
                &spawn_pwd,
                &origin,
            )
            .await
        {
            tracing::error!(
                "Failed to start video pipeline: {}",
                cgi::redact_creds(&e.to_string())
            );
        }
    });

    let product_info = format!(
        "<CGI_Result><code>0</code><model>IP Camera</model><ip>{}</ip><webPort>{}</webPort></CGI_Result>",
        camera.ip, camera.http_port
    );

    let video_resp = VideoOpenSuccess {
        channel: 0,
        dev_name: "camera".into(),
        privilege: 1,
        enable_talk: 0,
        enable_audio: 0,
        model_name: "IP Camera".into(),
        ip: camera.ip.clone(),
        web_port: camera.http_port,
        usr: resp_user,
        pwd: resp_pwd,
        product_all_info: product_info,
    };

    let response = WsMessage::with_response(RESPONSE_REQUEST_OPEN_VIDEO_SUCCESS, video_resp)?;
    Ok(serde_json::to_string(&response)?)
}

async fn handle_close_video(
    video: Option<&std::sync::Arc<VideoServer>>,
    group_id: u64,
) -> Result<String> {
    let Some(video) = video else {
        return Err(anyhow::anyhow!("no video session (login first)"));
    };
    tracing::info!("Close video: group={group_id}, stopping pipeline");
    video.stop().await;
    let response = WsMessage::new(RESPONSE_REQUEST_CLOSE_VIDEO);
    Ok(serde_json::to_string(&response)?)
}

fn handle_audio(msg: &WsMessage) -> Result<String> {
    let _cmd: Option<AudioCmd> = msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    tracing::info!("Audio request received");

    let response = WsMessage::new(RESPONSE_REQUEST_AUDIO);
    Ok(serde_json::to_string(&response)?)
}

fn handle_talk(msg: &WsMessage) -> Result<String> {
    let _cmd: Option<TalkCmd> = msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    tracing::info!("Talk request received");

    let talk_resp = CgiResponse {
        result: 0,
        response: String::new(),
    };
    let response = WsMessage::with_response(RESPONSE_REQUEST_TALK, talk_resp)?;
    Ok(serde_json::to_string(&response)?)
}

fn handle_record(msg: &WsMessage) -> Result<String> {
    let _cmd: Option<RecordCmd> = msg
        .cmd_object
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    tracing::info!("Record request received");

    let response = WsMessage::new(RESPONSE_REQUEST_RECORD);
    Ok(serde_json::to_string(&response)?)
}

/// Extract stream type from a message's cmdObject.
///
/// Protocol `streamType` is 0 (main) or 1 (sub), never exceeding `u32`.
#[allow(clippy::cast_possible_truncation)]
fn extract_stream_type(msg: &WsMessage) -> u32 {
    msg.cmd_object
        .as_ref()
        .and_then(|v| v.get("streamType").and_then(serde_json::Value::as_u64))
        .map_or(0, |v| v as u32)
}

/// Extract username from login cmdObject.
fn extract_login_user(msg: &WsMessage) -> Option<String> {
    msg.cmd_object
        .as_ref()
        .and_then(|v| v.get("usr").and_then(|s| s.as_str().map(str::to_string)))
}

/// Extract password from login cmdObject.
fn extract_login_pwd(msg: &WsMessage) -> Option<String> {
    msg.cmd_object
        .as_ref()
        .and_then(|v| v.get("pwd").and_then(|s| s.as_str().map(str::to_string)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Direct handler + dispatch coverage (no real camera required) ---

    /// A camera whose pinned `addr` points at a 127.0.0.1 mock. The SSRF
    /// guard only applies during login's `resolve_camera`, not to the handler
    /// dials, so handlers reach the mock directly.
    fn fake_camera(addr: std::net::SocketAddr) -> CameraInfo {
        CameraInfo {
            ip: "camera.lan".into(),
            http_port: 88,
            // Closed discard port: any pipeline spawn fails fast in the bg.
            rtsp_port: 9,
            addr,
        }
    }

    fn msg(msg_id: u32, cmd: serde_json::Value) -> WsMessage {
        WsMessage {
            version: 1,
            msg_id,
            group_id: 0,
            sequence: 1,
            data_len: 0,
            cmd_object: Some(cmd),
        }
    }

    /// A tiny fake camera answering every CGI/snapshot request with 200.
    async fn spawn_fake_camera() -> std::net::SocketAddr {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut br = tokio::io::BufReader::new(r);
                    let mut line = String::new();
                    if br.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if br.read_line(&mut h).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if h.trim().is_empty() {
                            break;
                        }
                    }
                    let (body, ctype) = if line.contains("snapPicture.jpg") {
                        (b"FAKEJPEG".to_vec(), "image/jpeg")
                    } else {
                        (
                            b"<CGI_Result><code>0</code></CGI_Result>".to_vec(),
                            "text/xml",
                        )
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = w.write_all(resp.as_bytes()).await;
                    let _ = w.write_all(&body).await;
                    let _ = w.flush().await;
                });
            }
        });
        addr
    }

    #[test]
    fn pure_handlers_build_responses() {
        let s = handle_logout().unwrap();
        assert!(s.contains(&RESPONSE_REQUEST_LOGOUT.to_string()));
        let audio = handle_audio(&msg(WS_REQUEST_AUDIO, serde_json::json!({}))).unwrap();
        assert!(audio.contains(&RESPONSE_REQUEST_AUDIO.to_string()));
        let talk = handle_talk(&msg(WS_REQUEST_TALK, serde_json::json!({}))).unwrap();
        assert!(talk.contains(&RESPONSE_REQUEST_TALK.to_string()));
        let record = handle_record(&msg(WS_REQUEST_RECORD, serde_json::json!({}))).unwrap();
        assert!(record.contains(&RESPONSE_REQUEST_RECORD.to_string()));
    }

    #[tokio::test]
    async fn close_video_stops_pipeline_or_errors() {
        assert!(handle_close_video(None::<&Arc<VideoServer>>, 0)
            .await
            .is_err());
        let vs = VideoServer::new();
        let s = handle_close_video(Some(&vs), 0).await.unwrap();
        assert!(s.contains(&RESPONSE_REQUEST_CLOSE_VIDEO.to_string()));
    }

    #[tokio::test]
    async fn open_video_requires_camera_and_session() {
        let m = msg(WS_REQUEST_OPEN_VIDEO, serde_json::json!({ "streamType": 0 }));
        let cam = fake_camera("127.0.0.1:9".parse().unwrap());
        // No camera → error.
        assert!(handle_open_video(&m, Some("u"), Some("p"), None, Some(&VideoServer::new()), 0)
            .await
            .is_err());
        // No video session → error.
        assert!(handle_open_video(&m, Some("u"), Some("p"), Some(&cam), None, 0)
            .await
            .is_err());
        // Both present → success response (pipeline spawn is best-effort bg).
        let vs = VideoServer::new();
        let s =
            handle_open_video(&m, Some("u"), Some("p"), Some(&cam), Some(&vs), 0)
                .await
                .unwrap();
        assert!(s.contains(&RESPONSE_REQUEST_OPEN_VIDEO_SUCCESS.to_string()));
    }

    #[tokio::test]
    async fn cgi_and_snapshot_error_branches() {
        let cam = fake_camera("127.0.0.1:9".parse().unwrap());
        let cgi = msg(WS_REQUEST_CGI, serde_json::json!({ "cgi": "x" }));
        // Not authenticated.
        assert!(handle_cgi(&cgi, None, None, Some(&cam)).await.is_err());
        // No camera.
        assert!(handle_cgi(&cgi, Some("u"), Some("p"), None).await.is_err());
        // Invalid cmdObject.
        let bad = msg(WS_REQUEST_CGI, serde_json::json!(12345));
        assert!(handle_cgi(&bad, Some("u"), Some("p"), Some(&cam)).await.is_err());

        // Snapshot: not authenticated / no camera → Err.
        assert!(handle_snapshot(None, None, Some(&cam)).await.is_err());
        assert!(handle_snapshot(Some("u"), Some("p"), None).await.is_err());
        // Unreachable camera → dial fails → Ok with result -1 (not Err).
        let s = handle_snapshot(Some("u"), Some("p"), Some(&cam))
            .await
            .expect("snapshot swallows dial error");
        assert!(s.contains("-1"));
    }

    #[tokio::test]
    async fn login_failure_paths_are_fast() {
        let (pool, _) = fresh_pool(10).await;
        let session = SessionState::new();
        let matching = OriginInfo::parse("http://192.168.1.50:88").unwrap();
        let mismatch = OriginInfo::parse("http://camera.lan:88").unwrap();

        // Missing/invalid cmdObject → hard Err.
        let bad = msg(WS_REQUEST_LOGIN, serde_json::json!(42));
        assert!(handle_login(&bad, false, Some(&matching), &session, &pool)
            .await
            .is_err());

        // Blocked IP literal (loopback) → resolve fails → -1 response, no camera.
        let blocked = msg(
            WS_REQUEST_LOGIN,
            serde_json::json!({"ip": "127.0.0.1", "usr": "u", "pwd": "p", "webPort": 88}),
        );
        let a = handle_login(&blocked, false, Some(&matching), &session, &pool)
            .await
            .expect("failed login yields a response, not Err");
        assert!(!a.responses.is_empty());
        assert!(a.new_camera.is_none());

        // Resolvable LAN IP but no Origin → -1 response.
        let no_origin = msg(
            WS_REQUEST_LOGIN,
            serde_json::json!({"ip": "192.168.1.50", "usr": "u", "pwd": "p", "webPort": 88}),
        );
        let a = handle_login(&no_origin, false, None, &session, &pool)
            .await
            .expect("no-origin login yields a response");
        assert!(!a.responses.is_empty());
        assert!(a.new_camera.is_none());

        // Resolvable LAN IP + mismatched Origin → -1 response.
        let a = handle_login(&no_origin, false, Some(&mismatch), &session, &pool)
            .await
            .expect("mismatched-origin login yields a response");
        assert!(!a.responses.is_empty());
        assert!(a.new_camera.is_none());
    }

    #[tokio::test]
    async fn ensure_session_video_allocates_and_reuses() {
        let (pool, _) = fresh_pool(10).await;
        let mut session = SessionState::new();
        // No live_port → allocate fresh.
        let (port, new) = ensure_session_video(&session, &pool, 1).await.unwrap();
        assert!(new.is_some());
        // With live_port set → reuse, no new allocation.
        session.live_port = Some(port);
        let (p2, new2) = ensure_session_video(&session, &pool, 1).await.unwrap();
        assert_eq!(p2, port);
        assert!(new2.is_none());
    }

    #[tokio::test]
    async fn dispatch_routes_all_message_types() {
        let (pool, _) = fresh_pool(10).await;
        let cam = fake_camera(spawn_fake_camera().await);
        let session = SessionState {
            camera: Some(cam),
            user: Some("admin".into()),
            pwd: Some("test".into()),
            stream_type: 0,
            video: Some(VideoServer::new()),
            live_port: Some(20000),
            video_listener: None,
        };
        let origin = OriginInfo::parse("http://camera.lan:88").unwrap();

        // HELLO / heartbeat / unknown → no responses.
        for id in [HELLO_RESPONSE, HEARTBEAT_MSG_ID, 99_999_999] {
            let a = dispatch_message(&msg(id, serde_json::json!({})), &session, Some(&origin), &pool)
                .await;
            assert!(a.responses.is_empty(), "msgId {id} should be silent");
        }

        // Each routed request type yields exactly one response.
        let cases: [(u32, serde_json::Value); 9] = [
            (WS_REQUEST_LOGOUT, serde_json::json!({})),
            (WS_REQUEST_CGI, serde_json::json!({ "cgi": "getDeviceStatus" })),
            (WS_REQUEST_CGI, serde_json::json!({ "cgi": "/cgi-bin/CGIProxy.fcgi?usr=a&pwd=b&cmd=streamSetting" })),
            (WS_REQUEST_SNAP, serde_json::json!({})),
            (WS_REQUEST_OPEN_VIDEO, serde_json::json!({ "streamType": 0 })),
            (WS_REQUEST_CLOSE_VIDEO, serde_json::json!({})),
            (WS_REQUEST_AUDIO, serde_json::json!({})),
            (WS_REQUEST_TALK, serde_json::json!({})),
            (WS_REQUEST_RECORD, serde_json::json!({})),
        ];
        for (id, cmd) in cases {
            let a = dispatch_message(&msg(id, cmd.clone()), &session, Some(&origin), &pool).await;
            assert_eq!(a.responses.len(), 1, "msgId {id} should respond once");
        }

        // Upgrade-prompt enable → one response.
        let a = dispatch_message(
            &msg(WS_REQUEST_UPGRADE_PROMPT_ENABLE, serde_json::json!({})),
            &session,
            Some(&origin),
            &pool,
        )
        .await;
        assert_eq!(a.responses.len(), 1);
    }

    #[test]
    fn test_extract_stream_type_main() {
        let msg = WsMessage::with_cmd(
            WS_REQUEST_OPEN_VIDEO,
            VideoPlayCmd {
                stream_type: 0,
                timeout: 5000,
            },
        )
        .unwrap();
        assert_eq!(extract_stream_type(&msg), 0);
    }

    #[test]
    fn test_extract_stream_type_sub() {
        let msg = WsMessage::with_cmd(
            WS_REQUEST_OPEN_VIDEO,
            VideoPlayCmd {
                stream_type: 1,
                timeout: 5000,
            },
        )
        .unwrap();
        assert_eq!(extract_stream_type(&msg), 1);
    }

    #[test]
    fn test_extract_stream_type_missing() {
        let msg = WsMessage::new(WS_REQUEST_OPEN_VIDEO);
        assert_eq!(extract_stream_type(&msg), 0);
    }

    #[test]
    fn test_camera_info_holds_pinned_addr() {
        let info = CameraInfo {
            ip: "camera.lan".into(),
            http_port: 88,
            rtsp_port: 554,
            addr: "192.168.1.50:88".parse().unwrap(),
        };
        // Claim fields are kept for logs/origin-gating; dials use `addr`.
        assert_eq!(info.ip, "camera.lan");
        assert_eq!(info.http_port, 88);
        assert_eq!(info.rtsp_port, 554);
        assert_eq!(info.addr.ip().to_string(), "192.168.1.50");
        assert_eq!(info.addr.port(), 88);
    }

    #[tokio::test]
    async fn test_login_pinning_rejects_blocked_claim() {
        // Claiming loopback must fail closed at the resolve step — the
        // pinned address is the only address later dials may use.
        let err = crate::net::resolve_camera("127.0.0.1", 88).await.unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn test_hello_message_serialization() {
        let hello = WsMessage::new(HELLO_CMD);
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("1000000"));
    }

    #[test]
    fn test_logout_response() {
        let response = WsMessage::new(RESPONSE_REQUEST_LOGOUT);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("50002"));
    }

    #[test]
    fn test_extract_login_credentials() {
        let login = LoginCmd {
            ip: None,
            ddns: None,
            uid: None,
            usr: "admin".into(),
            pwd: "secret".into(),
            web_port: None,
            media_port: None,
            ddns_media: None,
            mac: None,
            ipc_type: None,
            connect_type: None,
            stream_type: None,
            timeout: None,
            service_type: None,
        };
        let msg = WsMessage::with_cmd(WS_REQUEST_LOGIN, login).unwrap();
        assert_eq!(extract_login_user(&msg), Some("admin".into()));
        assert_eq!(extract_login_pwd(&msg), Some("secret".into()));
    }

    #[test]
    fn test_error_response() {
        let json = error_response(RESPONSE_REQUEST_CGI, "test error");
        assert!(json.contains("50006"));
        assert!(json.contains("-1"));
    }

    // --- End-to-end WS handshake tests (origin enforcement) ---

    use base64::Engine as _;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::http as ws_http;

    /// Connect a WS client to a fresh core server, complete the HELLO
    /// handshake, send a login for `claim_ip:claim_port`, and return the
    /// 50001/50026 login response plus the live stream for follow-ups.
    async fn login_roundtrip(
        origin: Option<&str>,
        claim_ip: &str,
        claim_port: u16,
    ) -> (
        serde_json::Value,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let port = listener.local_addr().unwrap().port();
        // Live-port pool rooted at a free ephemeral port (logins in these
        // tests never succeed past camera validation, so it's rarely used).
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe");
        let base = probe.local_addr().expect("probe addr").port();
        drop(probe);
        tokio::spawn(crate::core::run(
            listener,
            crate::video::ports::PortPool::new(base, 100),
        ));

        // A pre-built Request is used verbatim, so the WS handshake headers
        // must be supplied here (tungstenite only generates them for
        // string/Url requests).
        let key = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let mut req = ws_http::Request::builder()
            .method("GET")
            .uri(format!("ws://127.0.0.1:{port}/"))
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", key);
        if let Some(o) = origin {
            req = req.header("Origin", o);
        }
        let req = req.body(()).expect("build request");

        let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .expect("tcp connect");
        let (mut ws, _) = tokio_tungstenite::client_async(req, tcp)
            .await
            .expect("ws handshake");

        // HELLO from server
        let hello = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("hello timed out")
            .expect("stream ended")
            .expect("hello msg");
        let hello: WsMessage = msg_to_value(&hello)
            .and_then(|v| serde_json::from_value(v).ok())
            .expect("parse hello");
        assert_eq!(hello.msg_id, HELLO_CMD);

        // HELLO response
        let hr = WsMessage::new(HELLO_RESPONSE);
        ws.send(Message::Text(
            serde_json::to_string(&hr)
                .expect("serialize hello response")
                .into(),
        ))
        .await
        .expect("send hello response");

        // Login
        let login = WsMessage {
            version: 1,
            msg_id: WS_REQUEST_LOGIN,
            group_id: 1,
            sequence: 1,
            data_len: 0,
            cmd_object: Some(serde_json::json!({
                "ip": claim_ip,
                "usr": "admin",
                "pwd": "test",
                "webPort": claim_port,
                "mediaPort": claim_port,
                "timeout": 5000,
            })),
        };
        ws.send(Message::Text(
            serde_json::to_string(&login)
                .expect("serialize login")
                .into(),
        ))
        .await
        .expect("send login");

        // Read until the login response (50001)
        let resp = loop {
            let m = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
                .await
                .expect("login response timed out")
                .expect("stream ended")
                .expect("login msg");
            let v = msg_to_value(&m).expect("parse login response");
            if v["msgid"] == serde_json::json!(RESPONSE_REQUEST_LOGIN) {
                break v;
            }
        };
        (resp, ws)
    }

    fn msg_to_value(m: &Message) -> Option<serde_json::Value> {
        let s = match m {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8(b.to_vec()).ok()?,
            _ => return None,
        };
        serde_json::from_str(&s).ok()
    }

    #[tokio::test]
    async fn test_ws_login_origin_mismatch_rejected() {
        let (resp, _ws) =
            login_roundtrip(Some("http://evil.example"), "camera.lan", 88).await;
        assert_eq!(resp["result"], serde_json::json!(-1));
        let text = resp["response"].as_str().unwrap_or("");
        assert!(
            text.contains("origin does not match claimed camera"),
            "unexpected: {text}"
        );
    }

    #[tokio::test]
    async fn test_ws_login_missing_origin_rejected() {
        let (resp, _ws) = login_roundtrip(None, "camera.lan", 88).await;
        assert_eq!(resp["result"], serde_json::json!(-1));
        let text = resp["response"].as_str().unwrap_or("");
        assert!(text.contains("no origin"), "unexpected: {text}");
    }

    #[tokio::test]
    async fn test_ws_login_origin_match_proceeds() {
        // Origin matches the claim; the camera (127.0.0.1:9, discard port)
        // is then rejected by address validation — proving the origin gate
        // passed and the session stays unarmed.
        let (resp, mut ws) = login_roundtrip(Some("http://127.0.0.1:9"), "127.0.0.1", 9).await;
        assert_eq!(resp["result"], serde_json::json!(-1));
        let text = resp["response"].as_str().unwrap_or("");
        assert!(text.contains("camera unreachable"), "unexpected: {text}");

        // CGI on the unarmed session must be refused, not proxied.
        let cgi = WsMessage {
            version: 1,
            msg_id: WS_REQUEST_CGI,
            group_id: 1,
            sequence: 2,
            data_len: 0,
            cmd_object: Some(serde_json::json!({"cgi": "getDevInfo", "timeout": 5000})),
        };
        ws.send(Message::Text(
            serde_json::to_string(&cgi).expect("serialize cgi").into(),
        ))
        .await
        .expect("send cgi");
        let m = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("cgi response timed out")
            .expect("stream ended")
            .expect("cgi msg");
        let v = msg_to_value(&m).expect("parse cgi response");
        assert_eq!(v["msgid"], serde_json::json!(RESPONSE_REQUEST_CGI));
        // error_response nests the payload in cmdObject
        let obj = v.get("cmdObject").unwrap_or(&v);
        assert_eq!(obj["result"], serde_json::json!(-1));
        let text = obj["response"].as_str().unwrap_or("");
        assert!(
            text.contains("not authenticated") || text.contains("no camera configured"),
            "unexpected: {text}"
        );
    }

    #[test]
    fn test_origin_parse_lan_host_with_path() {
        // Browsers may include the path in Origin for nested pages; we only
        // care about host:port.
        let o = OriginInfo::parse("http://camera.lan:88/live").unwrap();
        assert_eq!(o.host, "camera.lan");
        assert_eq!(o.port, 88);
    }

    #[test]
    fn test_origin_parse_lan_host_with_port() {
        let o = OriginInfo::parse("http://camera.lan:88").unwrap();
        assert_eq!(o.host, "camera.lan");
        assert_eq!(o.port, 88);
    }

    #[test]
    fn test_origin_parse_default_ports() {
        assert_eq!(OriginInfo::parse("http://192.168.1.50").unwrap().port, 80);
        assert_eq!(OriginInfo::parse("https://cam.lan").unwrap().port, 443);
        assert_eq!(OriginInfo::parse("https://cam.lan:443").unwrap().port, 443);
    }

    #[test]
    fn test_origin_parse_ipv6() {
        let o = OriginInfo::parse("http://[::1]:88").unwrap();
        assert_eq!(o.host, "::1");
        assert_eq!(o.port, 88);
    }

    #[test]
    fn test_origin_parse_rejects_garbage() {
        assert!(OriginInfo::parse("").is_none());
        assert!(OriginInfo::parse("null").is_none());
        assert!(OriginInfo::parse("ftp://cam:88").is_none());
        assert!(OriginInfo::parse("http://").is_none());
        assert!(OriginInfo::parse("http://cam:99999").is_none());
        assert!(OriginInfo::parse("http://cam:notaport").is_none());
    }

    #[test]
    fn test_origin_matches() {
        let o = OriginInfo::parse("http://camera.lan:88").unwrap();
        assert!(o.matches("camera.lan", 88));
        assert!(o.matches("CAMERA.LAN ", 88)); // case + trim
        assert!(!o.matches("192.168.1.50", 88));
        assert!(!o.matches("camera.lan", 80));
        assert!(!o.matches("evil.example", 88));
    }

    #[test]
    fn test_failed_login_action_is_unarmed() {
        let action = failed_login_action(RESPONSE_REQUEST_LOGIN, "no origin").unwrap();
        assert!(action.new_camera.is_none());
        assert!(action.new_auth.is_none());
        assert_eq!(action.responses.len(), 1);
        assert!(action.responses[0].contains("\"result\":-1"));
        assert!(action.responses[0].contains("no origin"));
    }

    #[test]
    fn test_session_state_defaults() {
        let session = SessionState::new();
        assert!(session.camera.is_none());
        assert!(session.user.is_none());
        assert!(session.pwd.is_none());
    }

    #[test]
    fn test_session_logout_clears_state() {
        let mut session = SessionState::new();
        session.camera = Some(CameraInfo {
            ip: "1.2.3.4".into(),
            http_port: 80,
            rtsp_port: 88,
            addr: "1.2.3.4:80".parse().unwrap(),
        });
        session.user = Some("admin".into());
        session.pwd = Some("pass".into());

        session.logout();

        assert!(session.camera.is_none());
        assert!(session.user.is_none());
        assert!(session.pwd.is_none());
    }

    // --- Per-session video isolation (Phase 3) ---

    use std::sync::Arc;

    async fn fresh_pool(count: u16) -> (Arc<PortPool>, u16) {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe");
        let base = probe.local_addr().expect("probe addr").port();
        drop(probe);
        (PortPool::new(base, count), base)
    }

    /// Arm a session's pipeline the way a successful login + openVideo
    /// would (dialing a dead discard port so the pipeline task fails
    /// harmlessly in the background — `start` succeeds synchronously).
    async fn arm_session(
        session: &mut SessionState,
        pool: &Arc<PortPool>,
        camera_host: &str,
    ) -> (u16, Arc<VideoServer>) {
        let port = pool.alloc().await.expect("port");
        let video = VideoServer::new();
        let listener = tokio::spawn({
            let v = video.clone();
            async move {
                let _ = crate::video::http_flv::run(port, v).await;
            }
        });
        session.video = Some(video.clone());
        session.live_port = Some(port);
        session.video_listener = Some(listener);
        video
            .start("127.0.0.1", 9, 0, "u", "p", &OriginInfo { host: camera_host.into(), port: 88 })
            .await
            .expect("arm pipeline");
        (port, video)
    }

    #[tokio::test]
    async fn teardown_stops_only_its_pipeline_and_releases_port() {
        let (pool, _base) = fresh_pool(10).await;
        let (mut a, mut b) = (SessionState::new(), SessionState::new());
        let (port_a, vs_a) = arm_session(&mut a, &pool, "cam-a.lan").await;
        let (port_b, vs_b) = arm_session(&mut b, &pool, "cam-b.lan").await;
        assert_eq!(pool.in_use().await, std::collections::HashSet::from([port_a, port_b]));
        assert!(vs_a.has_video());
        assert!(vs_b.has_video());

        // Session A's WS connection drops.
        a.teardown_video(&pool, "127.0.0.1:1").await;

        // A: pipeline stopped, port released, state cleared.
        assert!(!vs_a.has_video());
        assert!(vs_a.armed_origin().is_none());
        assert!(a.video.is_none() && a.live_port.is_none());
        // B: untouched.
        assert!(vs_b.has_video());
        assert!(vs_b.armed_origin().is_some());
        assert_eq!(pool.in_use().await, std::collections::HashSet::from([port_b]));
        assert_eq!(pool.in_use_count().await, 1);

        b.teardown_video(&pool, "127.0.0.1:2").await;
        assert_eq!(pool.in_use_count().await, 0);
    }

    #[tokio::test]
    async fn close_video_of_session_a_leaves_session_b_running() {
        let (pool, _base) = fresh_pool(10).await;
        let (mut a, mut b) = (SessionState::new(), SessionState::new());
        let (_pa, vs_a) = arm_session(&mut a, &pool, "cam-a.lan").await;
        let (_pb, vs_b) = arm_session(&mut b, &pool, "cam-b.lan").await;

        // Session A sends closeVideo.
        let resp =
            handle_close_video(Some(a.video.as_ref().expect("video")), 1).await.unwrap();
        assert!(resp.contains("50011"), "unexpected: {resp}");
        assert!(!vs_a.has_video());
        assert!(vs_b.has_video());

        // Session B closes too.
        handle_close_video(Some(b.video.as_ref().expect("video")), 2).await.unwrap();
        assert!(!vs_b.has_video());
    }

    #[tokio::test]
    async fn video_requests_without_login_are_refused() {
        let empty = SessionState::new();
        let err = handle_close_video(empty.video.as_ref(), 0).await.unwrap_err();
        assert!(err.to_string().contains("no video session"));

        let open_msg = WsMessage::with_cmd(
            WS_REQUEST_OPEN_VIDEO,
            VideoPlayCmd {
                stream_type: 0,
                timeout: 5000,
            },
        )
        .unwrap();
        let err = handle_open_video(&open_msg, None, None, None, None, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no camera"));

        // With a camera but no video session (pool-exhaustion path):
        let camera = CameraInfo {
            ip: "cam.lan".into(),
            http_port: 88,
            rtsp_port: 88,
            addr: "1.2.3.4:88".parse().unwrap(),
        };
        let err = handle_open_video(&open_msg, Some("u"), Some("p"), Some(&camera), None, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no video session"));
    }
}
