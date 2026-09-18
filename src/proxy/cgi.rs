/// CGI proxy: forwards WebSocket CGI commands to the camera's HTTP CGI interface.
///
/// ## Address handling (Phase 2 hardening)
///
/// Every function takes the camera as a **pre-validated, pinned**
/// [`SocketAddr`]. The address was resolved and validated exactly once at
/// login (see `crate::core::handle_login` / `crate::net::resolve_camera`);
/// re-resolving per dial would open a DNS-rebinding window in which later
/// dials — carrying credentials in the query string — could be steered at a
/// rogue host. This module never resolves hostnames and never follows
/// redirects (see `crate::proxy::shared_client`).
use anyhow::{Context, Result};
use std::net::SocketAddr;

use crate::protocol::messages::{CgiResponse, InitInfo};
use crate::proxy::{read_capped, shared_client};

/// Proxy a CGI command to the camera and return the XML response.
///
/// Uses the camera's native CGI API:
/// - `cmd` = command name (e.g., `logIn`, `getDevInfo`)
/// - `usrName` / `pwd` = credentials (API uses these names, not `usr`)
/// - `privilege` = privilege level (required for `logIn`, 1-2)
pub async fn proxy_cgi(
    camera: SocketAddr,
    user: &str,
    pwd: &str,
    cmd: &str,
) -> Result<CgiResponse> {
    // Build the CGIProxy.fcgi URL with API-correct parameter names.
    let url = format!("http://{camera}/cgi-bin/CGIProxy.fcgi?usrName={user}&pwd={pwd}&cmd={cmd}");

    tracing::debug!("CGI proxy: GET {}", redact_creds(&url));

    let resp = shared_client()
        .get(&url)
        .send()
        .await
        .context("failed to connect to camera")?;

    cgi_result(resp).await
}

/// Proxy a CGI command using the full path string from the JS client.
///
/// The JS sends the CGI string as a full path:
/// `/cgi-bin/CGIProxy.fcgi?usr=...&pwd=...&cmd=...&other=...`
/// The caller (core.rs) has already stripped the leading slash and verified
/// the path prefix; the host is the pinned, login-validated camera address.
pub async fn proxy_cgi_full_path(camera: SocketAddr, path: &str) -> Result<CgiResponse> {
    let url = format!("http://{camera}/{path}");
    tracing::debug!("CGI proxy (full path): GET {}", redact_creds(&url));

    let resp = shared_client()
        .get(&url)
        .send()
        .await
        .context("failed to connect to camera")?;

    cgi_result(resp).await
}

/// Proxy a CGI command using `usr` parameter (used by non-auth commands).
///
/// Some CGI endpoints (`getDevInfo`, `getProductAllInfo`, `getImageSetting`, etc.)
/// use `usr` instead of `usrName`.
pub async fn proxy_cgi_usr(
    camera: SocketAddr,
    user: &str,
    pwd: &str,
    cmd: &str,
) -> Result<CgiResponse> {
    let url = format!("http://{camera}/cgi-bin/CGIProxy.fcgi?usr={user}&pwd={pwd}&cmd={cmd}");
    tracing::debug!("CGI proxy (usr): GET {}", redact_creds(&url));

    let resp = shared_client()
        .get(&url)
        .send()
        .await
        .context("failed to connect to camera")?;

    cgi_result(resp).await
}

/// Proxy a login command to the camera.
///
/// The camera API requires `cmd=logIn` (camelCase) and `privilege` parameter.
pub async fn proxy_login(camera: SocketAddr, user: &str, pwd: &str) -> Result<CgiResponse> {
    let url = format!(
        "http://{camera}/cgi-bin/CGIProxy.fcgi?usrName={user}&pwd={pwd}&cmd=logIn&privilege=2"
    );
    tracing::debug!("Login: GET {}", redact_creds(&url));

    let resp = shared_client()
        .get(&url)
        .send()
        .await
        .context("failed to connect to camera")?;

    let cgi_resp = cgi_result(resp).await?;

    // Log without credentials: the response body is camera XML (no creds),
    // but keep it short.
    tracing::debug!(
        "Login: camera returned {:?}",
        &cgi_resp.response[..cgi_resp.response.len().min(200)]
    );

    Ok(cgi_resp)
}

/// Proxy a logout command to the camera.
pub async fn proxy_logout(camera: SocketAddr, user: &str, pwd: &str) -> Result<CgiResponse> {
    proxy_cgi(camera, user, pwd, "logOut").await
}

/// Proxy a snapshot request to the camera.
pub async fn proxy_snapshot(camera: SocketAddr, user: &str, pwd: &str) -> Result<String> {
    let url = format!("http://{camera}/cgi-bin/snapPicture.jpg?usr={user}&pwd={pwd}");

    tracing::debug!("Snapshot: GET {}", redact_creds(&url));

    let resp = shared_client()
        .get(&url)
        .send()
        .await
        .context("failed to connect to camera for snapshot")?;

    let bytes = read_capped(resp)
        .await
        .context("failed to read snapshot data")?;
    Ok(base64_encode(&bytes))
}

/// Turn a camera HTTP response into a `CgiResponse`, capping the body size.
///
/// Non-2xx statuses (including `302` — redirects are never followed, see
/// `crate::proxy::shared_client`) map to a `result: -1` CGI result.
async fn cgi_result(resp: reqwest::Response) -> Result<CgiResponse> {
    let status = resp.status();
    let body = read_capped(resp).await?;
    let body = String::from_utf8_lossy(&body).to_string();

    if status.is_success() {
        Ok(CgiResponse {
            result: 0,
            response: body,
        })
    } else {
        tracing::warn!("CGI proxy: camera returned HTTP {status}");
        Ok(CgiResponse {
            result: -1,
            response: format!(
                "<CGI_Result><code>-1</code><error>HTTP {status}</error></CGI_Result>"
            ),
        })
    }
}

/// Mask credential values (`pwd`, `usr`, `usrName`, `user`) anywhere in a
/// string so camera credentials never reach the logs in plaintext.
///
/// Error chains from reqwest embed the full request URL (credentials in the
/// query string), so this is applied to both URLs and error text.
pub fn redact_creds(s: &str) -> String {
    const KEYS: [&str; 4] = ["pwd", "usrName", "usr", "user"];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // Earliest key occurrence wins; on a tie, the longer/earlier-listed
        // key ("usrName" before "usr") wins.
        let mut earliest: Option<(usize, &str)> = None;
        for key in KEYS {
            if let Some(p) = rest.find(key) {
                if earliest.map_or(true, |(ep, _)| p < ep) {
                    earliest = Some((p, key));
                }
            }
        }
        let Some((p, key)) = earliest else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..p]);
        let after_key = &rest[p + key.len()..];
        match after_key.strip_prefix('=') {
            Some(value) => {
                let terminators = ['&', ')', ' ', '\t', '"', '\'', '\n', '\r'];
                let end = value.find(terminators).unwrap_or(value.len());
                out.push_str(key);
                out.push_str("=***");
                rest = &value[end..];
            }
            None => {
                // Key name without '=' (e.g. "user" inside "username") — copy verbatim.
                out.push_str(key);
                rest = after_key;
            }
        }
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(data)
}

/// Gather post-login data from the camera and build init info.
///
/// Queries multiple CGI endpoints to populate the `InitInfo` struct
/// that the JS `message100` handler needs to complete the login flow.
/// Unavailable fields use sensible defaults.
pub async fn gather_init_info(
    camera: SocketAddr,
    user: &str,
    pwd: &str,
    http_flv_port: u16,
) -> (Option<String>, InitInfo) {
    // Gather data from multiple CGI endpoints in parallel
    let product_info = proxy_cgi_usr(camera, user, pwd, "getProductAllInfo").await;
    let stream_param = proxy_cgi_usr(camera, user, pwd, "getVideoStreamParam").await;
    let image_setting = proxy_cgi_usr(camera, user, pwd, "getImageSetting").await;
    let audio_setting = proxy_cgi_usr(camera, user, pwd, "getAudioSetting").await;
    let infra_led = proxy_cgi_usr(camera, user, pwd, "getInfraLedConfig").await;

    // Parse stream parameters (4 streams × 5 fields = 20 values)
    let stream_param: Vec<u32> = parse_stream_param(stream_param.as_ref().ok());

    // Parse image settings
    let (brightness, contrast, hue, saturation, sharpness) =
        parse_image_setting(image_setting.as_ref().ok());

    // Parse audio settings
    let volume = parse_audio_setting(audio_setting.as_ref().ok());

    // Parse infra LED mode
    let infra_led_mode = parse_infra_led(infra_led.as_ref().ok());

    // Extract product info XML for 50008 response
    let product_xml = product_info.ok().map(|r| r.response);

    // Build InitInfo (50009 response)
    let init_info = InitInfo {
        result: 0,
        rtmp_port: 0,
        live_port: http_flv_port,
        record_state: 0,
        flash_enable_buffer: 0,
        is_mute: 0,
        volume,
        led_state: 0,
        preset_point_cnt: 0,
        cruise_map_cnt: 0,
        cru_cruise_map: 0,
        main_stream_type: 0,
        sub_stream_type: 0,
        stream_param,
        brightness,
        contrast,
        hue,
        saturation,
        sharpness,
        is_mirror: 0,
        is_flip: 0,
        is_alarming: 0,
        alarm_type: 0,
        pwr_freq: 0,
        infra_led_mode,
        infra_led_state: 0,
        usr_privilege: 2,
    };

    (product_xml, init_info)
}

/// Parse stream parameters from getVideoStreamParam XML.
/// Returns 20 values: 4 streams × 5 fields (resolution, bitRate, frameRate, GOP, isVBR).
fn parse_stream_param(resp: Option<&CgiResponse>) -> Vec<u32> {
    let Some(resp) = resp else {
        return default_stream_param();
    };

    let mut params = Vec::with_capacity(20);
    // Parse the XML response for resolution0-3, bitRate0-3, frameRate0-3, GOP0-3, isVBR0-3
    // For each stream index (0-3), extract all 5 fields
    for i in 0..4 {
        let resolution = extract_xml_value(&resp.response, &format!("resolution{i}"));
        let bit_rate = extract_xml_value(&resp.response, &format!("bitRate{i}"));
        let frame_rate = extract_xml_value(&resp.response, &format!("frameRate{i}"));
        let gop = extract_xml_value(&resp.response, &format!("GOP{i}"));
        let is_vbr = extract_xml_value(&resp.response, &format!("isVBR{i}"));
        params.push(resolution.unwrap_or(9));
        params.push(bit_rate.unwrap_or(2_097_152));
        params.push(frame_rate.unwrap_or(15));
        params.push(gop.unwrap_or(30));
        params.push(is_vbr.unwrap_or(1));
    }
    params
}

/// Parse image settings from getImageSetting XML.
fn parse_image_setting(resp: Option<&CgiResponse>) -> (u32, u32, u32, u32, u32) {
    let Some(resp) = resp else {
        return (50, 47, 55, 20, 30);
    };
    (
        extract_xml_value(&resp.response, "brightness").unwrap_or(50),
        extract_xml_value(&resp.response, "contrast").unwrap_or(47),
        extract_xml_value(&resp.response, "hue").unwrap_or(55),
        extract_xml_value(&resp.response, "saturation").unwrap_or(20),
        extract_xml_value(&resp.response, "sharpness").unwrap_or(30),
    )
}

/// Parse audio settings from getAudioSetting XML.
fn parse_audio_setting(resp: Option<&CgiResponse>) -> u32 {
    let Some(resp) = resp else {
        return 100;
    };
    extract_xml_value(&resp.response, "volume").unwrap_or(100)
}

/// Parse infra LED mode from getInfraLedConfig XML.
fn parse_infra_led(resp: Option<&CgiResponse>) -> u32 {
    let Some(resp) = resp else {
        return 0;
    };
    extract_xml_value(&resp.response, "mode").unwrap_or(0)
}

/// Simple XML value extractor using string search.
/// Looks for `<tag>value</tag>` pattern in the response.
fn extract_xml_value(xml: &str, tag: &str) -> Option<u32> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?.checked_add(open.len())?;
    let rest = &xml[start..];
    let end = rest.find(&close)?;
    rest[..end].trim().parse().ok()
}

/// Default stream parameters when CGI is unavailable.
fn default_stream_param() -> Vec<u32> {
    vec![
        9, 2_097_152, 15, 30, 1, // stream 0: 1080p, 2Mbps, 15fps, GOP30, VBR
        7, 1_048_576, 15, 30, 1, // stream 1: 720p, 1Mbps, 15fps, GOP30, VBR
        0, 524_288, 15, 30, 1, // stream 2: D1, 512kbps, 15fps, GOP30, VBR
        7, 4_194_304, 25, 30, 1, // stream 3: 720p, 4Mbps, 25fps, GOP30, VBR
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind a one-shot fake camera HTTP server on loopback that answers the
    /// first request with the given raw HTTP response (status line, headers,
    /// body). Loopback is fine here: the proxy layer dials a pre-validated
    /// `SocketAddr` and never performs address validation itself.
    async fn fake_camera(raw_response: String) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(raw_response.as_bytes()).await;
        });
        addr
    }

    #[tokio::test]
    async fn proxy_cgi_dials_pinned_addr_and_roundtrips() {
        let xml = "<CGI_Result><code>0</code><model>5019</model></CGI_Result>";
        let addr = fake_camera(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
            xml.len()
        ))
        .await;

        let resp = proxy_cgi(addr, "admin", "secret", "getDevInfo")
            .await
            .unwrap();
        assert_eq!(resp.result, 0);
        assert_eq!(resp.response, xml);
    }

    #[tokio::test]
    async fn proxy_cgi_does_not_follow_redirects() {
        // A 302 pointing at a second loopback "victim" listener. If redirect
        // following were enabled, the victim would see a connection.
        let victim = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let victim_addr = victim.local_addr().unwrap();
        let victim_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_clone = victim_seen.clone();
        tokio::spawn(async move {
            if victim.accept().await.is_ok() {
                seen_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let addr = fake_camera(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{victim_addr}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;

        let resp = proxy_cgi(addr, "admin", "secret", "getDevInfo")
            .await
            .unwrap();
        assert_eq!(resp.result, -1);
        assert!(resp.response.contains("HTTP 302"), "{}", resp.response);
        // Give any (incorrect) redirect-follow a moment to connect.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !victim_seen.load(std::sync::atomic::Ordering::SeqCst),
            "redirect Location was dialed"
        );
    }

    #[tokio::test]
    async fn proxy_cgi_full_path_dials_pinned_addr() {
        let xml = "<CGI_Result><code>0</code></CGI_Result>";
        let addr = fake_camera(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
            xml.len()
        ))
        .await;

        let resp = proxy_cgi_full_path(addr, "cgi-bin/CGIProxy.fcgi?usr=admin&pwd=x&cmd=getDevInfo")
            .await
            .unwrap();
        assert_eq!(resp.result, 0);
        assert_eq!(resp.response, xml);
    }

    #[tokio::test]
    async fn proxy_login_roundtrip() {
        let xml = "<CGI_Result><code>0</code><privilege>2</privilege></CGI_Result>";
        let addr = fake_camera(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
            xml.len()
        ))
        .await;

        let resp = proxy_login(addr, "admin", "secret").await.unwrap();
        assert_eq!(resp.result, 0);
        assert!(resp.response.contains("privilege"));
    }

    #[tokio::test]
    async fn proxy_snapshot_base64_roundtrip() {
        // Binary body, so no `fake_camera` (String-based) — raw listener.
        let jpg = b"\xff\xd8\xff\xe0fake-jpeg-bytes";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_bytes = jpg.to_vec();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body_bytes.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body_bytes).await;
        });

        let b64 = proxy_snapshot(addr, "admin", "secret").await.unwrap();
        let decoded = base64_decode(&b64);
        assert_eq!(decoded, jpg);
    }

    fn base64_decode(s: &str) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        STANDARD.decode(s).unwrap()
    }

    #[test]
    fn test_base64_encode() {
        let data = b"Hello, world!";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "SGVsbG8sIHdvcmxkIQ==");
    }

    #[test]
    fn test_base64_encode_empty() {
        let data = b"";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "");
    }

    #[test]
    fn test_redact_creds_masks_url_params() {
        let url = "http://192.168.1.50:88/cgi-bin/CGIProxy.fcgi?usrName=admin&pwd=secret&cmd=logIn";
        assert_eq!(
            redact_creds(url),
            "http://192.168.1.50:88/cgi-bin/CGIProxy.fcgi?usrName=***&pwd=***&cmd=logIn"
        );
    }

    #[test]
    fn test_redact_creds_usr_param() {
        let url = "http://192.168.1.50:88/cgi-bin/snapPicture.jpg?usr=admin&pwd=s3cr3t";
        assert_eq!(
            redact_creds(url),
            "http://192.168.1.50:88/cgi-bin/snapPicture.jpg?usr=***&pwd=***"
        );
    }

    #[test]
    fn test_redact_creds_error_chain_with_url() {
        // Shape of a reqwest error: the full URL (with creds) in the text.
        let e = "error sending request for url (http://127.0.0.1:88/cgi-bin/CGIProxy.fcgi?usrName=admin&pwd=hunter2&cmd=logIn)";
        assert_eq!(
            redact_creds(e),
            "error sending request for url (http://127.0.0.1:88/cgi-bin/CGIProxy.fcgi?usrName=***&pwd=***&cmd=logIn)"
        );
    }

    #[test]
    fn test_redact_creds_no_creds_unchanged() {
        let s = "http://192.168.1.50:88/ nothing sensitive here";
        assert_eq!(redact_creds(s), s);
    }

    #[test]
    fn test_redact_creds_username_not_masked() {
        // "user" must not match inside "username" (no '=' follows "user").
        let s = "username=bob&pwd=secret";
        assert_eq!(redact_creds(s), "username=bob&pwd=***");
    }

    #[test]
    fn test_cgi_response_success() {
        let resp = CgiResponse {
            result: 0,
            response: "<CGI_Result><code>0</code></CGI_Result>".into(),
        };
        let json = serde_json::to_string(&resp).expect("should serialize");
        assert!(json.contains("\"result\":0"));
    }

    #[test]
    fn test_cgi_response_error() {
        let resp = CgiResponse {
            result: -1,
            response: "error".into(),
        };
        let json = serde_json::to_string(&resp).expect("should serialize");
        assert!(json.contains("\"result\":-1"));
    }

    // --- XML parse helpers (pure) ---

    #[test]
    fn extract_xml_value_present_absent_and_bad() {
        assert_eq!(extract_xml_value("<a>123</a>", "a"), Some(123));
        // Whitespace around the value is trimmed.
        assert_eq!(extract_xml_value("<a>  456  </a>", "a"), Some(456));
        // Absent tag.
        assert_eq!(extract_xml_value("<a>1</a>", "b"), None);
        // Non-numeric value.
        assert_eq!(extract_xml_value("<a>abc</a>", "a"), None);
        // Empty value.
        assert_eq!(extract_xml_value("<a></a>", "a"), None);
        // Negative parses fine as i64? no — u32: negative fails to parse.
        assert_eq!(extract_xml_value("<a>-5</a>", "a"), None);
    }

    #[test]
    fn default_stream_param_has_20_known_values() {
        let p = default_stream_param();
        assert_eq!(p.len(), 20);
        assert_eq!(&p[0..5], &[9, 2_097_152, 15, 30, 1]);
        assert_eq!(&p[5..10], &[7, 1_048_576, 15, 30, 1]);
    }

    #[test]
    fn parse_stream_param_none_returns_default() {
        assert_eq!(parse_stream_param(None), default_stream_param());
    }

    #[test]
    fn parse_stream_param_parses_all_fields() {
        let mut xml = String::from("<CGI_Result><code>0</code>");
        for i in 0..4u32 {
            xml += &format!(
                "<resolution{i}>{r}</resolution{i}><bitRate{i}>{b}</bitRate{i}><frameRate{i}>{f}</frameRate{i}><GOP{i}>{g}</GOP{i}><isVBR{i}>{v}</isVBR{i}>",
                r = 100 + i, b = 1000 + i, f = 5 + i, g = 10 + i, v = i
            );
        }
        xml += "</CGI_Result>";
        let resp = CgiResponse { result: 0, response: xml };
        let p = parse_stream_param(Some(&resp));
        assert_eq!(p.len(), 20);
        for i in 0..4u32 {
            assert_eq!(p[5 * i as usize], 100 + i);
            assert_eq!(p[5 * i as usize + 1], 1000 + i);
            assert_eq!(p[5 * i as usize + 2], 5 + i);
            assert_eq!(p[5 * i as usize + 3], 10 + i);
            assert_eq!(p[5 * i as usize + 4], i);
        }
    }

    #[test]
    fn parse_stream_param_missing_fields_use_per_stream_defaults() {
        // A response with NO recognizable tags: every stream falls back to the
        // per-stream defaults (9, 2097152, 15, 30, 1), not the varied
        // default_stream_param().
        let resp = CgiResponse {
            result: 0,
            response: "<CGI_Result><code>0</code></CGI_Result>".into(),
        };
        let p = parse_stream_param(Some(&resp));
        for i in 0..4 {
            assert_eq!(&p[5 * i..5 * i + 5], &[9, 2_097_152, 15, 30, 1]);
        }
    }

    #[test]
    fn parse_image_setting_none_and_some() {
        assert_eq!(parse_image_setting(None), (50, 47, 55, 20, 30));
        let resp = CgiResponse {
            result: 0,
            response: "<brightness>66</brightness><contrast>44</contrast><hue>55</hue><saturation>22</saturation><sharpness>33</sharpness>".into(),
        };
        assert_eq!(parse_image_setting(Some(&resp)), (66, 44, 55, 22, 33));
    }

    #[test]
    fn parse_audio_setting_none_and_some() {
        assert_eq!(parse_audio_setting(None), 100);
        let resp = CgiResponse {
            result: 0,
            response: "<volume>77</volume>".into(),
        };
        assert_eq!(parse_audio_setting(Some(&resp)), 77);
        // Missing tag falls back to 100.
        let resp2 = CgiResponse {
            result: 0,
            response: "<other>1</other>".into(),
        };
        assert_eq!(parse_audio_setting(Some(&resp2)), 100);
    }

    #[test]
    fn parse_infra_led_none_and_some() {
        assert_eq!(parse_infra_led(None), 0);
        let resp = CgiResponse {
            result: 0,
            response: "<mode>2</mode>".into(),
        };
        assert_eq!(parse_infra_led(Some(&resp)), 2);
    }

    // --- async wrappers + aggregator (multi-request fake camera) ---

    /// A fake camera that answers many requests, dispatching on the `cmd=`
    /// query param. Each request gets a fresh connection (we reply
    /// `Connection: close`), which matches how reqwest dials the pinned addr.
    async fn fake_camera_multi(cmd_bodies: Vec<(&'static str, String)>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let bodies = cmd_bodies.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut br = BufReader::new(r);
                    loop {
                        let mut req = String::new();
                        if br.read_line(&mut req).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let cmd = req
                            .split("cmd=")
                            .nth(1)
                            .and_then(|s| s.split(['&', '\r', '\n', ' ']).next())
                            .unwrap_or("");
                        let body = bodies
                            .iter()
                            .find(|(c, _)| *c == cmd)
                            .map(|(_, b)| b.clone())
                            .unwrap_or_else(|| "<CGI_Result><code>0</code></CGI_Result>".to_string());
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        if w.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                        w.flush().await.ok();
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn proxy_cgi_usr_roundtrip() {
        let xml = "<CGI_Result><code>0</code><ok>1</ok></CGI_Result>";
        let addr = fake_camera_multi(vec![("getProductAllInfo", xml.to_string())]).await;
        let resp = proxy_cgi_usr(addr, "admin", "secret", "getProductAllInfo")
            .await
            .unwrap();
        assert_eq!(resp.result, 0);
        assert_eq!(resp.response, xml);
    }

    #[tokio::test]
    async fn proxy_logout_roundtrip() {
        let xml = "<CGI_Result><code>0</code></CGI_Result>";
        let addr = fake_camera_multi(vec![("logOut", xml.to_string())]).await;
        let resp = proxy_logout(addr, "admin", "secret").await.unwrap();
        assert_eq!(resp.result, 0);
    }

    #[tokio::test]
    async fn gather_init_info_parses_all_endpoints() {
        let addr = fake_camera_multi(vec![
            ("getProductAllInfo", "<CGI_Result><code>0</code><product>FOO</product></CGI_Result>".into()),
            (
                "getVideoStreamParam",
                "<CGI_Result><code>0</code><resolution0>9</resolution0><bitRate0>2097152</bitRate0><frameRate0>15</frameRate0><GOP0>30</GOP0><isVBR0>1</isVBR0></CGI_Result>".into(),
            ),
            (
                "getImageSetting",
                "<brightness>66</brightness><contrast>44</contrast><hue>55</hue><saturation>22</saturation><sharpness>33</sharpness>".into(),
            ),
            ("getAudioSetting", "<volume>77</volume>".into()),
            ("getInfraLedConfig", "<mode>2</mode>".into()),
        ])
        .await;

        let (product_xml, info) = gather_init_info(addr, "admin", "secret", 9999).await;

        assert!(product_xml.as_deref().unwrap().contains("<product>FOO</product>"));
        assert_eq!(info.live_port, 9999);
        assert_eq!(info.volume, 77);
        assert_eq!(info.infra_led_mode, 2);
        assert_eq!(
            (info.brightness, info.contrast, info.hue, info.saturation, info.sharpness),
            (66, 44, 55, 22, 33)
        );
        assert_eq!(&info.stream_param[0..5], &[9, 2_097_152, 15, 30, 1]);
    }

    #[tokio::test]
    async fn gather_init_info_tolerates_failed_endpoints() {
        // No camera listening: every proxy_cgi_usr fails (connection refused),
        // so gather falls back to defaults for every field.
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let (product_xml, info) = gather_init_info(dead, "admin", "secret", 7777).await;
        assert!(product_xml.is_none());
        assert_eq!(info.live_port, 7777);
        assert_eq!(info.volume, 100); // audio default
        assert_eq!(info.infra_led_mode, 0); // infra default
        assert_eq!(info.stream_param, default_stream_param());
        assert_eq!(info.brightness, 50); // image default
    }
}
