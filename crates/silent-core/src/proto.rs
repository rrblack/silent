//! Wire formats.
//!
//! Three layers, from outside in:
//!
//! 1. [`ServerMsg`] / [`ClientMsg`]: JSON over the relay WebSocket. The relay
//!    sees room numbers, peer ids and opaque `data` strings, nothing else.
//! 2. [`PeerFrame`]: what goes inside `data`. Either a cleartext SPAKE2 message
//!    or a sealed envelope.
//! 3. [`Signal`]: the plaintext inside a sealed envelope (names, SDP, ICE).
//!
//! Once WebRTC is up, [`ChannelMsg`] travels over the DTLS data channel.

use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::handshake::PeerId;

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_PEERS: usize = 4;
pub const MAX_RELAY_DATA: usize = 64 * 1024;
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_CHAT_CHARS: usize = 4000;
/// Join attempts a room accepts in a burst. Every attempt is one online
/// guess at the code, so relays refill this budget slowly: one attempt per
/// [`JOIN_REFILL_SECS`].
pub const JOIN_BURST: u32 = 20;
pub const JOIN_REFILL_SECS: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Create,
    Join,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Create => "create",
            Intent::Join => "join",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Intent::Create),
            "join" => Some(Intent::Join),
            _ => None,
        }
    }
}

pub fn room_path(nameplate: u16, intent: Intent) -> String {
    format!("/v1/rooms/{nameplate}?intent={}", intent.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        you: PeerId,
        peers: Vec<PeerId>,
        #[serde(default)]
        ice: IceConfig,
    },
    Joined {
        peer: PeerId,
    },
    Left {
        peer: PeerId,
    },
    Relay {
        from: PeerId,
        data: String,
    },
    Error {
        code: RelayErrorCode,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Relay { to: PeerId, data: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum RelayErrorCode {
    #[error("room is full")]
    RoomFull,
    #[error("room number already in use")]
    RoomTaken,
    #[error("no room with that number")]
    NoRoom,
    #[error("bad request")]
    BadRequest,
    #[error("message too large")]
    TooLarge,
    #[error("too many messages")]
    RateLimited,
    /// The room has had too many join attempts recently (see
    /// [`JOIN_BURST`]).
    #[error("too many attempts to join this room")]
    TooManyJoins,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerFrame {
    Pake(Vec<u8>),
    Sealed(Vec<u8>),
}

#[derive(Debug, thiserror::Error)]
#[error("malformed peer frame")]
pub struct FrameError;

impl PeerFrame {
    pub fn encode(&self) -> String {
        match self {
            PeerFrame::Pake(b) => format!("p:{}", B64.encode(b)),
            PeerFrame::Sealed(b) => format!("s:{}", B64.encode(b)),
        }
    }

    pub fn decode(s: &str) -> Result<Self, FrameError> {
        let (kind, body) = s.split_once(':').ok_or(FrameError)?;
        let bytes = B64.decode(body).map_err(|_| FrameError)?;
        match kind {
            "p" => Ok(PeerFrame::Pake(bytes)),
            "s" => Ok(PeerFrame::Sealed(bytes)),
            _ => Err(FrameError),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Signal {
    Hello {
        version: u32,
        name: String,
        /// See [`new_session_id`].
        session: String,
        /// The sender has a hardware video encoder and would like to send
        /// H.264. Each side sends with the first codec the other lists, so
        /// the receiver puts H.264 first for this pair.
        #[serde(default)]
        send_h264: bool,
    },
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        mid: String,
        mline: i32,
        candidate: String,
    },
    /// Asks the offering side to restart ICE (the answerer can't offer).
    Restart,
    Bye,
}

/// A random id a device keeps for the life of one room and sends only inside
/// the sealed `Hello`. When a device reconnects to the relay it gets a new
/// peer id, and this is how friends recognise it as the same person.
pub fn new_session_id() -> String {
    let bytes: [u8; 16] = rand::random();
    B64.encode(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ChannelMsg {
    Chat { text: String, sent_at_ms: u64 },
    Media(MediaState),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaState {
    pub mic: bool,
    pub camera: bool,
    pub screen: bool,
}

/// Sent in the relay's welcome. Lets the relay hand out short-lived TURN
/// credentials without them ever being baked into the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceConfig {
    pub ice_servers: Vec<IceServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl Default for IceConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![IceServerConfig {
                urls: vec![
                    "stun:stun.cloudflare.com:3478".into(),
                    "stun:stun.l.google.com:19302".into(),
                ],
                username: None,
                credential: None,
            }],
        }
    }
}

/// Trims and caps a user-supplied display name.
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Friend".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_msg_json_shape() {
        let m: ServerMsg = serde_json::from_str(r#"{"t":"welcome","you":2,"peers":[1]}"#).unwrap();
        assert_eq!(
            m,
            ServerMsg::Welcome {
                you: 2,
                peers: vec![1],
                ice: IceConfig::default()
            }
        );
        let e = ServerMsg::Error {
            code: RelayErrorCode::RoomFull,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"error","code":"room_full"}"#
        );
        let c: ClientMsg = serde_json::from_str(r#"{"t":"relay","to":3,"data":"x"}"#).unwrap();
        assert_eq!(
            c,
            ClientMsg::Relay {
                to: 3,
                data: "x".into()
            }
        );
    }

    #[test]
    fn peer_frame_roundtrip() {
        for f in [
            PeerFrame::Pake(vec![1, 2, 3]),
            PeerFrame::Sealed(vec![]),
            PeerFrame::Sealed(vec![9; 100]),
        ] {
            assert_eq!(PeerFrame::decode(&f.encode()).unwrap(), f);
        }
        assert!(PeerFrame::decode("x:AAAA").is_err());
        assert!(PeerFrame::decode("nocolon").is_err());
        assert!(PeerFrame::decode("p:!!!").is_err());
    }

    #[test]
    fn session_ids_are_random() {
        let a = new_session_id();
        assert_eq!(a.len(), 22);
        assert_ne!(a, new_session_id());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Kyle \n"), "Kyle");
        assert_eq!(clean_name(""), "Friend");
        assert_eq!(clean_name(&"x".repeat(100)).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn room_path_format() {
        assert_eq!(room_path(42, Intent::Join), "/v1/rooms/42?intent=join");
        assert_eq!(Intent::parse("create"), Some(Intent::Create));
        assert_eq!(Intent::parse("nope"), None);
    }
}
