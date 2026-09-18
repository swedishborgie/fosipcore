//! WebSocket message ID constants, matching the camera's web UI client (`js/var.js`).
//!
//! These are the exact values the browser's JS client sends and expects.
//! Many are defined here for future implementation phases.

// --- Client → Server requests ---
/// Service manager: request a core WebSocket port.
pub const CMD_REQUEST_PORT: u32 = 20000;
/// Authenticate with camera credentials.
pub const WS_REQUEST_LOGIN: u32 = 20001;
/// Logout.
pub const WS_REQUEST_LOGOUT: u32 = 20002;
/// Open audio stream.
pub const WS_REQUEST_AUDIO: u32 = 20003;
/// Talkback microphone.
pub const WS_REQUEST_TALK: u32 = 20004;
/// CGI proxy request (primary path for all config operations).
pub const WS_REQUEST_CGI: u32 = 20005;
/// Take a snapshot.
pub const WS_REQUEST_SNAP: u32 = 20006;
/// Open video stream.
pub const WS_REQUEST_OPEN_VIDEO: u32 = 20007;
/// Close video stream.
pub const WS_REQUEST_CLOSE_VIDEO: u32 = 20008;
/// Client-side file picker.
pub const WS_REQUEST_SELECT_FILE: u32 = 20009;
/// Client-side file size check.
pub const WS_REQUEST_GET_FILE_SIZE: u32 = 20010;
/// Firmware upgrade.
pub const WS_REQUEST_FIRMWARE_UPGRADE: u32 = 20011;
/// Search for camera IP and port.
pub const WS_REQUEST_SEARCH_DEV_IP_AND_PORT: u32 = 20012;
/// Import config file.
pub const WS_REQUEST_IMPORT_CONFIG_FILE: u32 = 20013;
/// Save record path.
pub const WS_REQUEST_SAVE_RECORD_PATH: u32 = 20014;
/// Get record path.
pub const WS_REQUEST_GET_RECORD_PATH: u32 = 20015;
/// Open record path.
pub const WS_REQUEST_OPEN_RECORD_PATH: u32 = 20016;
/// Start/stop recording.
pub const WS_REQUEST_RECORD: u32 = 20017;
/// Select record path.
pub const WS_REQUEST_SELECT_RECORD_PATH: u32 = 20018;
/// SD card management.
pub const WS_REQUEST_SD_MANAGER: u32 = 20019;
/// Re-login after reconnection.
pub const WS_REQUEST_LOGIN_AGAIN: u32 = 20020;
/// Enable/disable upgrade prompt.
pub const WS_REQUEST_UPGRADE_PROMPT_ENABLE: u32 = 20021;

// --- Control commands (legacy, may not be used by WS client) ---

/// Legacy login control command.
pub const CTRL_CMD_LOGIN: u32 = 30000;
/// Legacy logout control command.
pub const CTRL_CMD_LOGOUT: u32 = 30001;

// --- Server → Client responses ---

/// Service manager: port assignment response.
pub const RESPONSE_REQUEST_PORT: u32 = 50000;
/// Login response.
pub const RESPONSE_REQUEST_LOGIN: u32 = 50001;
/// Logout response.
pub const RESPONSE_REQUEST_LOGOUT: u32 = 50002;
/// Release response.
pub const RESPONSE_REQUEST_RELEASE: u32 = 50003;
/// Audio response.
pub const RESPONSE_REQUEST_AUDIO: u32 = 50004;
/// Talk response.
pub const RESPONSE_REQUEST_TALK: u32 = 50005;
/// CGI proxy response.
pub const RESPONSE_REQUEST_CGI: u32 = 50006;
/// Snapshot response.
pub const RESPONSE_REQUEST_SNAP: u32 = 50007;
/// Product info response.
pub const RESPONSE_REQUEST_LOGIN_PRODUCT_INFO: u32 = 50008;
/// Init info response.
pub const RESPONSE_REQUEST_LOGIN_MSG_INIT_INFO: u32 = 50009;
/// Open video response.
pub const RESPONSE_REQUEST_OPEN_VIDEO: u32 = 50010;
/// Close video response.
pub const RESPONSE_REQUEST_CLOSE_VIDEO: u32 = 50011;
/// Open video success response.
pub const RESPONSE_REQUEST_OPEN_VIDEO_SUCCESS: u32 = 50012;
/// Select file response.
pub const RESPONSE_REQUEST_SELECT_FILE: u32 = 50013;
/// Get file size response.
pub const RESPONSE_REQUEST_GET_FILE_SIZE: u32 = 50014;
/// Firmware upgrade fake CGI result.
pub const RESPONSE_REQUEST_FIRMWARE_UPGRADE_FAKE_CGI_RESULT: u32 = 50016;
/// Firmware upgrade message.
pub const RESPONSE_REQUEST_FIRMWARE_UPGRADE_MSG: u32 = 50017;
/// Search device IP/port response.
pub const RESPONSE_REQUEST_SEARCH_DEV_IP_AND_PORT: u32 = 50018;
/// Import config response.
pub const RESPONSE_REQUEST_IMPORT_CONFIG_FILE: u32 = 50019;
/// Save record path response.
pub const RESPONSE_REQUEST_SAVE_RECORD_PATH: u32 = 50020;
/// Get record path response.
pub const RESPONSE_REQUEST_GET_RECORD_PATH: u32 = 50021;
/// Open record path response.
pub const RESPONSE_REQUEST_OPEN_RECORD_PATH: u32 = 50022;
/// Record response.
pub const RESPONSE_REQUEST_RECORD: u32 = 50023;
/// Select record path response.
pub const RESPONSE_REQUEST_SELECT_RECORD_PATH: u32 = 50024;
/// SD manager response.
pub const RESPONSE_REQUEST_SD_MANAGER: u32 = 50025;
/// Login again response.
pub const RESPONSE_REQUEST_LOGIN_AGAIN: u32 = 50026;
/// Set upgrade prompt enable response.
pub const RESPONSE_REQUEST_SET_WEB_UPGRADE_PROMPT_ENABLE: u32 = 50027;
/// Get upgrade prompt enable response.
pub const RESPONSE_REQUEST_GET_WEB_UPGRADE_PROMPT_ENABLE: u32 = 50028;

// --- Server → Client events ---

/// Connection error event.
pub const EVENT_CONNECT_ERR: u32 = 60000;
/// Record open event.
pub const EVENT_RECORD_OPEN: u32 = 60001;
/// Record close event.
pub const EVENT_RECORD_CLOSE: u32 = 60002;
/// Image settings changed event.
pub const EVENT_IMAGE_CHANGE: u32 = 60003;
/// Cruise changed event.
pub const EVENT_CRUISE_CHANGE: u32 = 60004;
/// Preset changed event.
pub const EVENT_PRESET_CHANGE: u32 = 60005;
/// Mirror/flip changed event.
pub const EVENT_MIRROR_FLIP_CHANGE: u32 = 60006;
/// IR-CUT changed event.
pub const EVENT_IRCUT_CHANGE: u32 = 60007;
/// Stream param changed event.
pub const EVENT_STREAM_PARAM_CHANGE: u32 = 60008;
/// Stream type changed event.
pub const EVENT_STREAM_TYPE_CHANGE: u32 = 60009;
/// Video EPT state changed event.
pub const EVENT_VIDEO_EPT_STATE_CHANGE: u32 = 60010;
/// Power frequency changed event.
pub const EVENT_PWRFREQ_CHANGE: u32 = 60011;
/// Alarm changed event.
pub const EVENT_ALARM_CHANGE: u32 = 60012;
/// Sub stream type changed event.
pub const EVENT_SUB_STREAM_TYPE_CHANGE: u32 = 60013;
/// Sub stream param changed event.
pub const EVENT_SUB_STREAM_PARAM_CHANGE: u32 = 60014;
/// Get all product info event.
pub const EVENT_GET_ALL_PRODUCT_INFO: u32 = 60015;
/// HDR changed event.
pub const EVENT_HDR_CHANGE: u32 = 60016;
/// WDR changed event.
pub const EVENT_WDR_CHANGE: u32 = 60017;
/// NAA changed event.
pub const EVENT_NAA_CHANGE: u32 = 60018;
/// Guard position changed event.
pub const EVENT_GUARD_POSITION_CHANGE: u32 = 60019;
/// Record state event.
pub const EVENT_RECORD_STATE: u32 = 60021;
/// Import config event.
pub const EVENT_IMPORT_CONFIG: u32 = 60022;
/// FTP state event.
pub const EVENT_FTP_STATE: u32 = 60023;
/// Reconnect event.
pub const EVENT_RECONNECT: u32 = 60024;
/// Audio volume changed event.
pub const EVENT_AUDIO_VOLUME_CHANGE: u32 = 60025;
/// Compressor changed event.
pub const EVENT_COMPRESSOR_CHANGE: u32 = 60026;

// --- Handshake & internal ---

/// Server → Client: hello on connect (core WS).
pub const HELLO_CMD: u32 = 1_000_000;
/// Client → Server: hello response.
pub const HELLO_RESPONSE: u32 = 1_000_001;
/// Flash buffer full notification.
pub const FLASH_BUFFER_FULL: u32 = 1_000_002;
/// Flash buffer empty notification.
pub const FLASH_BUFFER_EMPTY: u32 = 1_000_003;
/// Web tell quit.
pub const WEB_TELL_QUIT: u32 = 1_000_004;
/// Flash reconnect RTMP.
pub const FLASH_CONNECT_RTMP_AGAIN: u32 = 1_000_005;
/// Flash buffer length.
pub const FLASH_BUFFER_LENGTH: u32 = 1_000_006;
/// Seek offset.
pub const SEEK_OFFSET: u32 = 1_000_007;

// --- Misc constants from var.js ---

/// Service manager WebSocket port.
pub const SERVICE_MANAGER_PORT: u16 = 50000;
/// Heartbeat message ID (sent by client on connect and periodically).
pub const HEARTBEAT_MSG_ID: u32 = 99999;
/// H264 codec.
pub const FOSIPC_H264: u32 = 0;
/// Motion JPEG codec.
pub const FOSIPC_MJ: u32 = 1;
/// P2P connection type.
pub const FOSCNTYPE_P2P: u32 = 0;
/// IP connection type.
pub const FOSCNTYPE_IP: u32 = 1;
