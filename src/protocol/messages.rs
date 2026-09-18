/// WebSocket message types and serialization.
///
/// The camera's JS client uses JSON messages. Key observations from the JS:
/// - Requests are serialized with camelCase keys (`msgId`, `cmdObject`) via `JSON.stringify(wsdata())`
/// - However, `datalen` is sent as lowercase (inconsistent with the `wsdata` constructor which defines `dataLen`)
/// - The JS reads responses using lowercase `msgid` and camelCase for other fields (`dstPort`, `seviceVer`)
///
/// We use `#[serde(alias = "...")]` to accept both casings for robustness.
use serde::{Deserialize, Serialize};

/// The outer envelope for every WebSocket message.
///
/// Matches the `wsdata()` JavaScript constructor and its JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WsMessage {
    /// Protocol version (always 1).
    pub version: u32,
    /// Message type identifier.
    ///
    /// The JS sends `msgId` (camelCase) in requests but reads responses using
    /// `json.msgid` (lowercase). We serialize as lowercase to match what the
    /// JS expects on the wire, and accept both via alias.
    #[serde(rename = "msgid", alias = "msgId")]
    pub msg_id: u32,
    /// Timestamp-based random group identifier.
    #[serde(default)]
    pub group_id: u64,
    /// Message sequence number.
    #[serde(default)]
    pub sequence: u32,
    /// Length of data payload (legacy, often 0).
    #[serde(alias = "datalen", default)]
    pub data_len: usize,
    /// Command-specific payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd_object: Option<serde_json::Value>,
}

impl WsMessage {
    /// Create a new outbound message.
    pub fn new(msg_id: u32) -> Self {
        Self {
            version: 1,
            msg_id,
            group_id: 0,
            sequence: 0,
            data_len: 0,
            cmd_object: None,
        }
    }

    /// Create a message with a JSON payload wrapped in `cmdObject`.
    ///
    /// Used for REQUEST messages from the client. For RESPONSE messages,
    /// the JS reads fields at the top level, so use `with_response()` instead.
    pub fn with_cmd(msg_id: u32, cmd: impl Serialize) -> serde_json::Result<Self> {
        let cmd_object = serde_json::to_value(cmd)?;
        Ok(Self {
            version: 1,
            msg_id,
            group_id: 0,
            sequence: 0,
            data_len: 0,
            cmd_object: Some(cmd_object),
        })
    }

    /// Build a flat JSON response where cmd data is merged into the top level.
    ///
    /// The JS client reads response fields like `json.result`, `json.response`,
    /// `json.rtmpPort` at the top level of the message, NOT inside `cmdObject`.
    /// This method serializes the message envelope and merges cmd fields into it.
    pub fn with_response(
        msg_id: u32,
        cmd: impl Serialize,
    ) -> serde_json::Result<serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert("version".into(), 1.into());
        map.insert("msgid".into(), msg_id.into());
        map.insert("groupId".into(), 0.into());
        map.insert("sequence".into(), 0.into());
        map.insert("dataLen".into(), 0.into());
        let cmd_value = serde_json::to_value(cmd)?;
        if let Some(cmd_map) = cmd_value.as_object() {
            for (k, v) in cmd_map {
                map.insert(k.clone(), v.clone());
            }
        }
        Ok(serde_json::Value::Object(map))
    }
}

// --- Service Manager messages ---

/// Response to `CMD_REQUEST_PORT`: assigns a core WebSocket port.
///
/// Note: `seviceVer` preserves the original typo from the JS client.
#[derive(Debug, Clone, Serialize)]
pub struct PortAssignment {
    /// Dynamic port for the core WebSocket server.
    #[serde(rename = "dstPort")]
    pub dst_port: u16,
    /// Service version string (must match JS version check format).
    #[serde(rename = "seviceVer")]
    pub service_ver: String,
}

// --- Login messages ---

/// Login command from client (`WS_REQUEST_LOGIN` / `WS_REQUEST_LOGIN_AGAIN`).
///
/// The JS client sends these fields FLAT in the `WsMessage` (not inside `cmdObject`),
/// with camelCase keys (`webPort`, `mediaPort`, etc.).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCmd {
    pub ip: Option<String>,
    #[serde(default)]
    pub ddns: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    pub usr: String,
    pub pwd: String,
    #[serde(default)]
    pub web_port: Option<u16>,
    #[serde(default)]
    pub media_port: Option<u16>,
    #[serde(default)]
    pub ddns_media: Option<u16>,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub ipc_type: Option<u32>,
    #[serde(default)]
    pub connect_type: Option<u32>,
    #[serde(default)]
    pub stream_type: Option<u32>,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub service_type: Option<u32>,
}

/// Generic timeout-only payload (used by heartbeat, hello response, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct TimeoutCmd {
    #[serde(default)]
    pub timeout: u32,
}

// --- CGI messages ---

/// CGI proxy request (`WS_REQUEST_CGI` / msgId 20005).
///
/// Matches the `CGIStr()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct CgiCmd {
    /// CGI command string (e.g., `"setWifiSetting&isEnable=1&ssid=MyWiFi"`).
    pub cgi: String,
    #[serde(default)]
    pub timeout: u32,
}

/// CGI proxy response (`RESPONSE_REQUEST_CGI` / msgId 50006).
#[derive(Debug, Clone, Serialize)]
pub struct CgiResponse {
    /// 0 = success, non-zero = error.
    pub result: i32,
    /// XML response body from the camera.
    pub response: String,
}

// --- Post-login messages ---

/// Product info response (`RESPONSE_REQUEST_LOGIN_PRODUCT_INFO` / msgId 50008).
///
/// The JS `message502` handler parses `json.response` as an XML string containing
/// camera capabilities (model, audioFlag, ptFlag, etc.) via `XmlParseAmbarellaFlag`.
/// This is the raw XML from `getProductAllInfo` CGI.
#[derive(Debug, Clone, Serialize)]
pub struct ProductInfo {
    /// Raw XML from `getProductAllInfo` CGI (e.g., `<CGI_Result>...<model>5019</model>...</CGI_Result>`).
    pub response: String,
}

/// Init info response (`RESPONSE_REQUEST_LOGIN_MSG_INIT_INFO` / msgId 50009).
///
/// The JS `message100` handler reads these fields and calls `PluginCallBack(100, ...)`
/// which sets `gVar.bLogin = true` and triggers video loading.
///
/// Data sourced from camera CGI commands: `getVideoStreamParam`, `getImageSetting`,
/// `getAudioSetting`, `getInfraLedConfig`, plus defaults for unavailable fields.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitInfo {
    /// 0 = success.
    pub result: i32,
    /// RTMP server port (0 if not available).
    pub rtmp_port: u16,
    /// HTTP FLV/HLS live port (0 if not available).
    pub live_port: u16,
    /// Record state (0 = not recording).
    pub record_state: u32,
    /// Flash enable buffer (0 = disabled).
    pub flash_enable_buffer: u32,
    /// 1 = muted, 0 = unmuted.
    pub is_mute: u32,
    /// Audio volume (0-100).
    pub volume: u32,
    /// IR LED state (0 = off, 1 = on).
    pub led_state: u32,
    /// Number of preset points (0 for fixed cameras).
    pub preset_point_cnt: u32,
    /// Number of cruise maps (0 for fixed cameras).
    pub cruise_map_cnt: u32,
    /// Current cruise map (0).
    pub cru_cruise_map: u32,
    /// Main stream codec type (0 = H264, 1 = MJPEG).
    pub main_stream_type: u32,
    /// Sub stream codec type (0 = H264, 1 = MJPEG).
    pub sub_stream_type: u32,
    /// Stream parameters: 4 streams × 5 fields each = 20 values.
    /// Fields per stream: resolution, bitRate, frameRate, GOP, isVBR.
    #[serde(rename = "streamParam")]
    pub stream_param: Vec<u32>,
    /// Image brightness (0-100).
    pub brightness: u32,
    /// Image contrast (0-100).
    pub contrast: u32,
    /// Image hue.
    pub hue: u32,
    /// Image saturation.
    pub saturation: u32,
    /// Image sharpness.
    pub sharpness: u32,
    /// 1 = mirrored, 0 = normal.
    pub is_mirror: u32,
    /// 1 = flipped, 0 = normal.
    pub is_flip: u32,
    /// 1 = alarm active, 0 = no alarm.
    pub is_alarming: u32,
    /// Alarm type (0 = none).
    pub alarm_type: u32,
    /// Power frequency (0 = 50Hz, 1 = 60Hz).
    pub pwr_freq: u32,
    /// IR LED mode (0 = auto, 1 = manual).
    pub infra_led_mode: u32,
    /// IR LED state (0 = off, 1 = on).
    pub infra_led_state: u32,
    /// User privilege level (2 = admin).
    pub usr_privilege: u32,
}

// --- Video messages ---

/// Video play request (`WS_REQUEST_OPEN_VIDEO` / msgId 20007).
///
/// Matches the `videoPlay()` JavaScript constructor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlayCmd {
    /// 0 = main stream, 1 = sub stream.
    #[serde(default)]
    pub stream_type: u32,
    #[serde(default)]
    pub timeout: u32,
}

/// Video open success response (`RESPONSE_REQUEST_OPEN_VIDEO_SUCCESS` / msgId 50012).
#[derive(Debug, Clone, Serialize)]
pub struct VideoOpenSuccess {
    pub channel: u32,
    pub dev_name: String,
    pub privilege: u32,
    pub enable_talk: u32,
    pub enable_audio: u32,
    pub model_name: String,
    pub ip: String,
    #[serde(rename = "webPort")]
    pub web_port: u16,
    pub usr: String,
    pub pwd: String,
    #[serde(rename = "productAllInfo")]
    pub product_all_info: String,
}

// --- Snapshot messages ---

/// Snapshot response (`RESPONSE_REQUEST_SNAP` / msgId 50007).
#[derive(Debug, Clone, Serialize)]
pub struct SnapResponse {
    pub result: i32,
    /// Base64-encoded JPEG image data.
    pub img: String,
}

// --- Audio messages ---

/// Audio control (`WS_REQUEST_AUDIO` / msgId 20003).
///
/// Matches the `Audio()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioCmd {
    /// 0 = main stream, 1 = sub stream.
    #[serde(default)]
    pub stream_type: u32,
    /// 1 = open, other = close.
    #[serde(default)]
    pub operation: u32,
    #[serde(default)]
    pub timeout: u32,
}

// --- Talk messages ---

/// Talkback control (`WS_REQUEST_TALK` / msgId 20004).
///
/// Matches the `Talk()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct TalkCmd {
    /// 1 = open, other = close.
    #[serde(default)]
    pub operation: u32,
    #[serde(default)]
    pub timeout: u32,
}

// --- Record messages ---

/// Record control (`WS_REQUEST_RECORD` / msgId 20017).
///
/// Matches the `Record()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordCmd {
    /// 1 = start, 0 = stop.
    #[serde(default)]
    pub operation: u32,
    #[serde(default)]
    pub r#type: u32,
    #[serde(default)]
    pub timeout: u32,
}

// --- SD Manager messages ---

/// SD card management (`WS_REQUEST_SD_MANAGER` / msgId 20019).
///
/// Matches the `SDManageCmd()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct SdManageCmd {
    #[serde(default)]
    pub timeout: u32,
    #[serde(default)]
    pub ip: Option<String>,
}

// --- Upgrade prompt messages ---

/// Upgrade prompt enable/disable (`WS_REQUEST_UPGRADE_PROMPT_ENABLE` / msgId 20021).
///
/// Matches the `upgradePromptEnableCmd()` JavaScript constructor.
#[derive(Debug, Clone, Deserialize)]
pub struct UpgradePromptCmd {
    #[serde(default)]
    pub operation: u32,
    #[serde(default)]
    pub plugin_ver: Option<String>,
    #[serde(default)]
    pub enable: Option<bool>,
}

// --- Init info response (msgId 50009) ---

/// Initial state pushed to client after login.
#[derive(Debug, Clone, Serialize)]
pub struct InitInfoResponse {
    pub result: i32,
    #[serde(rename = "rtmpPort")]
    pub rtmp_port: u16,
    #[serde(rename = "recordState")]
    pub record_state: u32,
    #[serde(rename = "flashEnableBuffer")]
    pub flash_enable_buffer: u32,
    #[serde(rename = "livePort")]
    pub live_port: u16,
    #[serde(rename = "isMute")]
    pub is_mute: u32,
    pub volume: u32,
    #[serde(rename = "ledState")]
    pub led_state: u32,
    #[serde(rename = "presetPointCnt")]
    pub preset_point_cnt: u32,
    #[serde(rename = "cruiseMapCnt")]
    pub cruise_map_cnt: u32,
    #[serde(rename = "cruCruiseMap")]
    pub cru_cruise_map: u32,
    #[serde(rename = "mainStreamType")]
    pub main_stream_type: u32,
    #[serde(rename = "subStreamType")]
    pub sub_stream_type: u32,
    pub brightness: u32,
    pub contrast: u32,
    pub hue: u32,
    pub saturation: u32,
    pub sharpness: u32,
    #[serde(rename = "isMirror")]
    pub is_mirror: u32,
    #[serde(rename = "isFlip")]
    pub is_flip: u32,
    #[serde(rename = "isAlarming")]
    pub is_alarming: u32,
    #[serde(rename = "alarmType")]
    pub alarm_type: u32,
    #[serde(rename = "pwrFreq")]
    pub pwr_freq: u32,
    #[serde(rename = "infraLedMode")]
    pub infra_led_mode: u32,
    #[serde(rename = "infraLedState")]
    pub infra_led_state: u32,
    #[serde(rename = "usrPrivilege")]
    pub usr_privilege: u32,
}

impl Default for InitInfoResponse {
    fn default() -> Self {
        Self {
            result: 0,
            rtmp_port: 0,
            record_state: 0,
            flash_enable_buffer: 0,
            live_port: 0,
            is_mute: 0,
            volume: 50,
            led_state: 0,
            preset_point_cnt: 0,
            cruise_map_cnt: 0,
            cru_cruise_map: 0,
            main_stream_type: 0,
            sub_stream_type: 0,
            brightness: 50,
            contrast: 50,
            hue: 0,
            saturation: 50,
            sharpness: 50,
            is_mirror: 0,
            is_flip: 0,
            is_alarming: 0,
            alarm_type: 0,
            pwr_freq: 0,
            infra_led_mode: 0,
            infra_led_state: 0,
            usr_privilege: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_request_port_message() {
        // JS sends: {version:1, msgId:20000, groupId:<num>, sequence:0, datalen:0}
        let json = r#"{"version":1,"msgId":20000,"groupId":123456789,"sequence":0,"datalen":0}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 20000);
        assert_eq!(msg.version, 1);
        assert_eq!(msg.group_id, 123456789);
        assert!(msg.cmd_object.is_none());
    }

    #[test]
    fn test_parse_cgi_request() {
        let json = r#"{"version":1,"msgId":20005,"groupId":987654321,"sequence":0,"dataLen":0,
            "cmdObject":{"cgi":"getDevInfo","timeout":5000}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 20005);
        let cmd = msg.cmd_object.as_ref().expect("should have cmdObject");
        let cgi: CgiCmd = serde_json::from_value(cmd.clone()).expect("should parse cgi cmd");
        assert_eq!(cgi.cgi, "getDevInfo");
        assert_eq!(cgi.timeout, 5000);
    }

    #[test]
    fn test_parse_login_request() {
        let json = r#"{"version":1,"msgId":20001,"groupId":111,"sequence":0,"dataLen":0,
            "cmdObject":{"ip":"192.168.1.100","usr":"admin","pwd":"secret",
            "webPort":88,"streamType":0,"timeout":5000,"connectType":1}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 20001);
        let cmd = msg.cmd_object.as_ref().expect("should have cmdObject");
        let login: LoginCmd = serde_json::from_value(cmd.clone()).expect("should parse login");
        assert_eq!(login.usr, "admin");
        assert_eq!(login.pwd, "secret");
    }

    #[test]
    fn test_parse_heartbeat() {
        // Heartbeat: {msgId: 99999, cmdObject: {timeout: 5000}}
        let json = r#"{"version":1,"msgId":99999,"cmdObject":{"timeout":5000}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 99999);
    }

    #[test]
    fn test_parse_hello_response() {
        let json = r#"{"version":1,"msgId":1000001,"cmdObject":{"timeout":5000}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 1000001);
    }

    #[test]
    fn test_serialize_port_assignment() {
        let resp = PortAssignment {
            dst_port: 50001,
            service_ver: "2.0.1.1".into(),
        };
        let json = serde_json::to_string(&resp).expect("should serialize");
        // Verify the JS-expected key names
        assert!(json.contains("\"dstPort\""));
        assert!(json.contains("\"seviceVer\""));
        assert!(json.contains("50001"));
    }

    #[test]
    fn test_full_port_response_serialization() {
        let port = PortAssignment {
            dst_port: 50001,
            service_ver: "2.0.1.1".into(),
        };
        let msg = WsMessage::with_cmd(50000, port).expect("should build");
        let json = serde_json::to_string(&msg).expect("should serialize");
        // JS reads json.msgid (lowercase) to route responses
        assert!(
            json.contains("\"msgid\":50000"),
            "expected lowercase 'msgid', got: {}",
            json
        );
        assert!(
            json.contains("\"dstPort\":50001"),
            "expected dstPort, got: {}",
            json
        );
        // Verify deserialization accepts camelCase msgId from JS client
        let js_msg = r#"{"version":1,"msgId":20001,"groupId":123,"sequence":0,"datalen":0}"#;
        let parsed: WsMessage = serde_json::from_str(js_msg).expect("should parse msgId");
        assert_eq!(parsed.msg_id, 20001);
    }

    #[test]
    fn test_serialize_cgi_response() {
        let resp = CgiResponse {
            result: 0,
            response: "<CGI_Result><code>0</code></CGI_Result>".into(),
        };
        let json = serde_json::to_string(&resp).expect("should serialize");
        assert!(json.contains("\"result\":0"));
        assert!(json.contains("CGI_Result"));
    }

    #[test]
    fn test_serialize_snap_response() {
        let resp = SnapResponse {
            result: 0,
            img: "base64data".into(),
        };
        let json = serde_json::to_string(&resp).expect("should serialize");
        assert!(json.contains("\"result\":0"));
        assert!(json.contains("\"img\":\"base64data\""));
    }

    #[test]
    fn test_message_new() {
        let msg = WsMessage::new(50000);
        assert_eq!(msg.msg_id, 50000);
        assert_eq!(msg.version, 1);
        assert!(msg.cmd_object.is_none());
    }

    #[test]
    fn test_message_with_cmd() {
        let port = PortAssignment {
            dst_port: 50001,
            service_ver: "2.0.1.1".into(),
        };
        let msg = WsMessage::with_cmd(50000, port).expect("should build");
        assert_eq!(msg.msg_id, 50000);
        assert!(msg.cmd_object.is_some());
    }

    #[test]
    fn test_roundtrip_ws_message() {
        let msg = WsMessage::new(20005);
        let json = serde_json::to_string(&msg).expect("should serialize");
        let parsed: WsMessage = serde_json::from_str(&json).expect("should parse back");
        assert_eq!(parsed.msg_id, 20005);
    }

    #[test]
    fn test_parse_video_open() {
        let json = r#"{"version":1,"msgId":20007,"groupId":123,"sequence":0,"dataLen":0,
            "cmdObject":{"streamType":0,"timeout":5000}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.msg_id, 20007);
        let cmd = msg.cmd_object.as_ref().expect("should have cmdObject");
        let video: VideoPlayCmd = serde_json::from_value(cmd.clone()).expect("should parse video");
        assert_eq!(video.stream_type, 0);
    }

    #[test]
    fn test_parse_video_open_defaults() {
        let json = r#"{"version":1,"msgId":20007,"cmdObject":{}}"#;
        let msg: WsMessage = serde_json::from_str(json).expect("should parse");
        let cmd = msg.cmd_object.as_ref().expect("should have cmdObject");
        let video: VideoPlayCmd = serde_json::from_value(cmd.clone()).expect("should parse");
        assert_eq!(video.stream_type, 0);
        assert_eq!(video.timeout, 0);
    }
}
