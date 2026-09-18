//! Outbound proxy helpers: the shared HTTP client used for every camera dial.
//!
//! ## Security properties (Phase 2 hardening)
//!
//! - **No redirects.** The stock camera CGI answers `200` and never issues
//!   redirects. Following them (reqwest's default) would let a plaintext-HTTP
//!   MITM, a DNS-rebound host, or a compromised camera answer `302` and have
//!   this process dial `169.254.169.254`, `127.0.0.1:<svc>`, etc. — defeating
//!   the resolved-address validation done at login.
//! - **Timeouts.** A blackholed address must not accumulate hung
//!   connections/tasks forever.
//! - **Capped response bodies.** A hostile camera response must not amplify
//!   memory without bound (`read_capped`).
pub mod cgi;

use anyhow::{bail, Context, Result};
use std::sync::OnceLock;
use std::time::Duration;

/// Hard cap on any single camera response body (CGI XML or snapshot JPEG).
/// A 2560×1440 JPEG snapshot is a few MB; 16 MiB is generous headroom.
pub const MAX_CAMERA_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Connect timeout for camera dials (seconds).
const CONNECT_TIMEOUT_SECS: u64 = 5;
/// Total timeout for camera dials (seconds). Legacy Foscam CGI can be slow,
/// but it never needs more than a few seconds.
const TOTAL_TIMEOUT_SECS: u64 = 15;

/// The single shared `reqwest::Client` for all camera dials.
///
/// One client process-wide: shared connection pool, uniform redirect/timeout
/// policy. `Policy::none()` is a load-bearing security choice — see the
/// module docs.
pub fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS))
            .build()
            // A failed client build means a broken TLS/native environment —
            // there is no sensible degraded mode for a proxy.
            .expect("failed to build shared reqwest client")
    })
}

/// Read a camera response body into memory with a hard size cap.
///
/// Checks `Content-Length` up front, then streams `chunk()` so a chunked or
/// lying body cannot exceed `MAX_CAMERA_RESPONSE_BYTES` in memory.
pub async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > MAX_CAMERA_RESPONSE_BYTES as u64 {
            bail!(
                "camera response too large: Content-Length {len} > {MAX_CAMERA_RESPONSE_BYTES} bytes"
            );
        }
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("failed to read camera response body")? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_CAMERA_RESPONSE_BYTES {
            bail!(
                "camera response too large: exceeded {MAX_CAMERA_RESPONSE_BYTES} bytes"
            );
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_capped_rejects_oversized_content_length() {
        // A tiny local server answering with a huge Content-Length.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let n = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            MAX_CAMERA_RESPONSE_BYTES as u64 + 1
                        )
                        .as_bytes(),
                    )
                    .await
                    .is_ok();
                if !n {
                    break;
                }
            }
        });

        let client = shared_client();
        let resp = client
            .get(format!("http://{addr}/cgi-bin/CGIProxy.fcgi"))
            .send()
            .await
            .unwrap();
        let err = read_capped(resp).await.unwrap_err().to_string();
        assert!(
            err.contains("too large"),
            "expected size rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn read_capped_streams_within_cap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = b"<CGI_Result><code>0</code></CGI_Result>";
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .ok();
            let _ = sock.write_all(body).await;
        });

        let client = shared_client();
        let resp = client
            .get(format!("http://{addr}/cgi-bin/CGIProxy.fcgi"))
            .send()
            .await
            .unwrap();
        let body = read_capped(resp).await.unwrap();
        assert_eq!(body, b"<CGI_Result><code>0</code></CGI_Result>");
    }
}
