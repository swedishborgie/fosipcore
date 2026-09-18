/// Service Manager WebSocket server.
///
/// Listens on port 50000 and responds to `CMD_REQUEST_PORT` (msgId 20000)
/// with the core WebSocket server's port assignment.
use anyhow::Result;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::protocol::constants::{CMD_REQUEST_PORT, RESPONSE_REQUEST_PORT};
use crate::protocol::messages::WsMessage;

/// Run the service manager WebSocket server.
///
/// `core_port` is the port the core WebSocket listener is bound on.
/// Every connection gets this same port — the core server handles
/// multiple simultaneous connections via Tokio's accept loop.
pub async fn run(listener: TcpListener, service_version: String, core_port: u16) -> Result<()> {
    tracing::info!("Service manager listening on {}", listener.local_addr()?);

    loop {
        let (socket, addr) = listener.accept().await?;
        tracing::info!("Service manager: connection from {}", addr);
        tokio::spawn(handle_connection(
            socket,
            service_version.clone(),
            core_port,
        ));
    }
}

async fn handle_connection(
    socket: tokio::net::TcpStream,
    service_version: String,
    core_port: u16,
) -> Result<()> {
    // Capture the Origin header for logging (the service manager only hands
    // out the core port — no camera address is involved, so no check here).
    let ws_stream = tokio_tungstenite::accept_hdr_async(socket, |
        request: &tokio_tungstenite::tungstenite::handshake::server::Request,
        response: tokio_tungstenite::tungstenite::handshake::server::Response,
    | {
        if let Some(v) = request.headers().get("Origin") {
            if let Ok(s) = v.to_str() {
                tracing::debug!("Service manager WS origin: {s}");
            }
        }
        Ok(response)
    })
    .await?;
    let (mut write, mut read): (
        SplitSink<WebSocketStream<tokio::net::TcpStream>, Message>,
        futures_util::stream::SplitStream<WebSocketStream<tokio::net::TcpStream>>,
    ) = ws_stream.split();

    while let Some(result) = read.next().await {
        let text: String = match result? {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Invalid UTF-8 in binary message: {}", e);
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
                tracing::warn!("Failed to parse message: {}", e);
                continue;
            }
        };

        match msg.msg_id {
            CMD_REQUEST_PORT => {
                tracing::info!("CMD_REQUEST_PORT received, returning core port {core_port}");
                // JS reads json.dstPort (flat, not inside cmdObject).
                // Binary UTF-8 — getJsonFromMessage wraps in Int8Array, then ab2str
                // reads each byte as a char code, so ASCII/UTF-8 maps correctly.
                let json = serde_json::json!({
                    "version": 1,
                    "msgid": RESPONSE_REQUEST_PORT,
                    "dstPort": core_port,
                    "seviceVer": service_version,
                })
                .to_string();
                write
                    .send(Message::Binary(json.into_bytes().into()))
                    .await?;
                tracing::info!("Returned core port {core_port}");
            }
            _ => {
                tracing::debug!("Unexpected msgId {} on service manager", msg.msg_id);
            }
        }
    }

    tracing::info!("Service manager: connection closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use tokio_tungstenite::tungstenite::http as ws_http;
    use tokio_tungstenite::WebSocketStream;

    type WsClient = WebSocketStream<tokio::net::TcpStream>;

    /// Start the service manager on an ephemeral loopback port and connect a
    /// WS client to it. `origin` is sent as the `Origin` handshake header when
    /// `Some`. Returns the connected client and the bound port.
    async fn connect_client(
        service_version: &str,
        core_port: u16,
        origin: Option<&str>,
    ) -> (WsClient, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run(
            listener,
            service_version.to_string(),
            core_port,
        ));

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
        let (ws, _) = tokio_tungstenite::client_async(req, tcp)
            .await
            .expect("ws handshake");
        (ws, port)
    }

    /// Serialize a CMD_REQUEST_PORT message with a given msgId.
    fn request_port_json(msg_id: u32) -> String {
        serde_json::json!({ "version": 1, "msgid": msg_id, "sequence": 1 })
            .to_string()
    }

    /// Receive the next message, converting Text/Binary UTF-8 into a JSON
    /// value. Returns the raw Message for control-frame assertions.
    async fn next_msg(
        ws: &mut WsClient,
    ) -> Message {
        tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("next timed out")
            .expect("stream ended")
            .expect("next msg")
    }

    #[tokio::test]
    async fn request_port_returns_core_port() {
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send");

        let reply = next_msg(&mut ws).await;
        let v = match &reply {
            Message::Binary(b) => serde_json::from_slice::<serde_json::Value>(b)
                .expect("parse reply"),
            Message::Text(t) => serde_json::from_str(t).expect("parse reply"),
            other => panic!("expected data frame, got {other:?}"),
        };
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
        assert_eq!(v["dstPort"], 43210);
        assert_eq!(v["seviceVer"], "2.4.1");
        assert_eq!(v["version"], 1);
    }

    #[tokio::test]
    async fn unknown_msg_id_is_ignored() {
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        // An unknown msgId gets no reply; the server must stay alive and
        // still answer a follow-up CMD_REQUEST_PORT.
        ws.send(Message::Text(request_port_json(99999).into()))
            .await
            .expect("send unknown");
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send valid");

        // The first frame out must be the real reply (the unknown one was
        // swallowed, not echoed).
        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
        assert_eq!(v["dstPort"], 43210);
    }

    #[tokio::test]
    async fn invalid_json_is_skipped() {
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Text("this is not json".into()))
            .await
            .expect("send garbage");
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send valid");

        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
        assert_eq!(v["dstPort"], 43210);
    }

    #[tokio::test]
    async fn binary_valid_utf8_is_answered() {
        // A valid-UTF-8 binary message is treated the same as text (the JS
        // client sends binary frames), so it must be parsed and answered.
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Binary(
            request_port_json(CMD_REQUEST_PORT).into_bytes().into(),
        ))
        .await
        .expect("send valid binary");
        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
        assert_eq!(v["dstPort"], 43210);
    }

    #[tokio::test]
    async fn binary_non_utf8_is_skipped() {
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        // 0xff 0xfe is not valid UTF-8.
        ws.send(Message::Binary(vec![0xff, 0xfe, 0x00, 0x01].into()))
            .await
            .expect("send non-utf8");
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send valid");

        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
        assert_eq!(v["dstPort"], 43210);
    }

    #[tokio::test]
    async fn ping_gets_pong() {
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Ping(vec![1, 2, 3].into()))
            .await
            .expect("send ping");

        let reply = next_msg(&mut ws).await;
        match reply {
            Message::Pong(payload) => assert_eq!(payload, vec![1, 2, 3]),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_terminates_cleanly() {
        let (mut ws, _port) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Close(None)).await.expect("send close");
        // The server's read loop breaks on Close and the handler returns. The
        // client side may surface a close-handshake error while reading; we
        // only care that the *listener* survives and accepts a fresh client.
        drop(ws);
        let (ws2, _) = connect_client("2.4.1", 43210, None).await;
        let mut ws2 = ws2;
        ws2.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send on reconnect");
        let reply = next_msg(&mut ws2).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
    }

    #[tokio::test]
    async fn origin_header_is_logged_and_answered() {
        // Sending an Origin header exercises the accept_hdr logging closure;
        // the server must still answer normally.
        let (mut ws, _) = connect_client("2.4.1", 43210, Some("http://camera.lan")).await;
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send");
        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
    }

    #[tokio::test]
    async fn client_pong_is_ignored() {
        // A Pong from the client is a control frame that must be skipped
        // without disrupting the loop.
        let (mut ws, _) = connect_client("2.4.1", 43210, None).await;
        ws.send(Message::Pong(vec![9, 9].into()))
            .await
            .expect("send pong");
        ws.send(Message::Text(request_port_json(CMD_REQUEST_PORT).into()))
            .await
            .expect("send valid");
        let reply = next_msg(&mut ws).await;
        let v = msg_to_value(&reply).expect("parse reply");
        assert_eq!(v["msgid"], RESPONSE_REQUEST_PORT);
    }

    fn msg_to_value(m: &Message) -> Option<serde_json::Value> {
        let s = match m {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8(b.to_vec()).ok()?,
            _ => return None,
        };
        serde_json::from_str(&s).ok()
    }
}
