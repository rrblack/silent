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
//! Once WebRTC is up, [`ChannelMsg`] travels over the DTLS data channel
//! [`CHAT_CHANNEL`], and files as [`FileFrame`]s over [`FILE_CHANNEL`].

use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::handshake::PeerId;

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_PEERS: usize = 4;
pub const MAX_RELAY_DATA: usize = 64 * 1024;
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_CHAT_CHARS: usize = 4000;
/// Negotiated data channel ids: chat and media state, and files (kept apart
/// so a big file never holds up a chat message).
pub const CHAT_CHANNEL: u16 = 0;
pub const FILE_CHANNEL: u16 = 1;
/// Largest file a peer accepts. Received files are kept in memory only.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Bytes of file data per data channel message.
pub const FILE_CHUNK: usize = 64 * 1024;
pub const MAX_FILE_NAME_CHARS: usize = 120;
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
    /// Fresh STUN/TURN servers, in answer to [`ClientMsg::RefreshIce`].
    Ice {
        ice: IceConfig,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Relay {
        to: PeerId,
        data: String,
    },
    /// Asks for fresh STUN/TURN servers: TURN credentials expire, and a
    /// long call may still need them to reconnect.
    RefreshIce,
    /// Asks the relay to remove `peer`, who failed the key exchange. The
    /// relay removes them once every peer that joined before them has asked,
    /// so a newcomer can never remove the people who were already there.
    Evict {
        peer: PeerId,
    },
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
    /// Everyone who was in the room first asked for this peer to be removed
    /// (see [`ClientMsg::Evict`]).
    #[error("removed from the room")]
    Evicted,
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

/// One binary message on [`FILE_CHANNEL`]: a kind byte, a 4-byte transfer
/// id, then the payload. The channel is ordered and reliable, so a `Start`
/// always arrives before its chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFrame {
    Start { id: u32, name: String, size: u64 },
    Chunk { id: u32, data: Vec<u8> },
    Cancel { id: u32 },
}

impl FileFrame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, id) = match self {
            FileFrame::Start { id, .. } => (0u8, id),
            FileFrame::Chunk { id, .. } => (1, id),
            FileFrame::Cancel { id } => (2, id),
        };
        let mut out = vec![kind];
        out.extend_from_slice(&id.to_be_bytes());
        match self {
            FileFrame::Start { name, size, .. } => {
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(name.as_bytes());
            }
            FileFrame::Chunk { data, .. } => out.extend_from_slice(data),
            FileFrame::Cancel { .. } => {}
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < 5 {
            return Err(FrameError);
        }
        let id = u32::from_be_bytes(bytes[1..5].try_into().expect("4 bytes"));
        let body = &bytes[5..];
        match bytes[0] {
            0 => {
                let size = body.get(..8).ok_or(FrameError)?;
                let size = u64::from_be_bytes(size.try_into().expect("8 bytes"));
                let name = std::str::from_utf8(&body[8..]).map_err(|_| FrameError)?;
                Ok(FileFrame::Start {
                    id,
                    name: clean_file_name(name),
                    size,
                })
            }
            1 => Ok(FileFrame::Chunk {
                id,
                data: body.to_vec(),
            }),
            2 => Ok(FileFrame::Cancel { id }),
            _ => Err(FrameError),
        }
    }
}

/// A file name that is safe to show and to offer as a default when saving:
/// no directories, no control characters, capped in length.
pub fn clean_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FILE_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned.to_string()
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
    fn file_frames_roundtrip() {
        for f in [
            FileFrame::Start {
                id: 7,
                name: "photo.jpg".into(),
                size: 123_456,
            },
            FileFrame::Chunk {
                id: u32::MAX,
                data: vec![1, 2, 3],
            },
            FileFrame::Chunk {
                id: 0,
                data: vec![],
            },
            FileFrame::Cancel { id: 9 },
        ] {
            assert_eq!(FileFrame::decode(&f.encode()).unwrap(), f);
        }
        assert!(FileFrame::decode(&[0, 0, 0, 0]).is_err());
        assert!(FileFrame::decode(&[0, 0, 0, 0, 1, 2]).is_err());
        assert!(FileFrame::decode(&[7, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(clean_file_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_file_name("C:\\Users\\me\\cat.png"), "cat.png");
        assert_eq!(clean_file_name(".bashrc"), "bashrc");
        assert_eq!(clean_file_name("a\nb.txt"), "ab.txt");
        assert_eq!(clean_file_name(""), "file");
        assert_eq!(clean_file_name("/"), "file");
        assert_eq!(
            clean_file_name(&"x".repeat(500)).chars().count(),
            MAX_FILE_NAME_CHARS
        );
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
