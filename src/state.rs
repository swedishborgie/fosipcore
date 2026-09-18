/// Per-session state types.
///
/// Used for tracking connection-level state that may be needed in later
/// phases (video streams, recording state, etc.). Currently the core
/// WebSocket handler uses its own local `SessionState` struct; these
/// types remain for potential shared-state needs in Phase 2+.
use std::collections::HashMap;

use tokio::sync::Mutex;

/// Authentication state for a session.
#[derive(Debug, Clone)]
pub struct SessionAuth {
    pub username: String,
    pub password: String,
    /// Camera IP from login command (if provided).
    pub camera_ip: Option<String>,
    /// Camera port from login command (if provided).
    pub camera_port: Option<u16>,
    /// Stream type preference (0 = main, 1 = sub).
    pub stream_type: u32,
}

/// Per-session state.
///
/// Five independent boolean flags tracking connection subsystems.
/// A bitmask would add complexity (external crate) for no gain at this scale.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct SessionState {
    /// Whether the client has completed the HELLO handshake.
    pub hello_completed: bool,
    /// Whether the client has authenticated.
    pub auth: Option<SessionAuth>,
    /// Active video stream (`stream_type`).
    pub video_stream: Option<u32>,
    /// Active audio stream.
    pub audio_enabled: bool,
    /// Active talkback.
    pub talk_enabled: bool,
    /// Recording state.
    pub recording: bool,
}

/// Global session registry: maps core port → session state.
pub type SessionRegistry = HashMap<u16, Mutex<SessionState>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_state_defaults() {
        let state = SessionState::default();
        assert!(!state.hello_completed);
        assert!(state.auth.is_none());
        assert!(state.video_stream.is_none());
        assert!(!state.audio_enabled);
        assert!(!state.talk_enabled);
        assert!(!state.recording);
    }

    #[test]
    fn test_session_auth() {
        let auth = SessionAuth {
            username: "admin".into(),
            password: "secret".into(),
            camera_ip: Some("192.168.1.100".into()),
            camera_port: Some(88),
            stream_type: 0,
        };
        assert_eq!(auth.username, "admin");
        assert_eq!(auth.camera_ip, Some("192.168.1.100".into()));
    }
}
