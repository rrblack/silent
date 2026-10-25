//! A room: one relay connection plus one WebRTC connection per friend.
//!
//! All state lives in a single actor task. The UI talks to it through
//! [`Room`] (commands in, [`RoomEvent`]s out); libwebrtc callbacks are turned
//! into internal messages so nothing is shared between threads.
//!
//! Per friend the sequence is:
//! 1. relay says they are here, both sides send a SPAKE2 message,
//! 2. both derive pair keys and send a sealed `Hello` (a wrong code shows up
//!    here as a message that fails to open),
//! 3. the newer peer (higher id) sends a sealed SDP offer, the other answers,
//!    ICE candidates follow sealed too,
//! 4. DTLS comes up using the fingerprints from those authenticated SDPs, so
//!    media and chat are end to end between the two devices.
//!
//! Reconnecting:
//! - A connection that drops is ICE-restarted by the offering side (the
//!   answerer asks for it with [`Signal::Restart`]).
//! - When the relay connection drops, calls keep going peer to peer and the
//!   room rejoins the relay by itself. Rejoining means a new relay id, so
//!   every pair starts over; the sealed session id in `Hello` lets friends
//!   swap the old connection for the new one without a "left" and "joined".
//! - Ids in [`RoomEvent`]s are local to this room and never reused, unlike
//!   relay ids (see [`Ids`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libwebrtc::peer_connection::PeerConnection;
use libwebrtc::peer_connection_factory::PeerConnectionFactory;
use libwebrtc::prelude::*;
use libwebrtc::video_track::RtcVideoTrack;
use silent_core::proto::{
    clean_file_name, clean_name, new_session_id, ChannelMsg, ClientMsg, FileFrame, IceConfig,
    Intent, MediaState, PeerFrame, RelayErrorCode, ServerMsg, Signal, CHAT_CHANNEL, FILE_CHANNEL,
    FILE_CHUNK, MAX_CHAT_CHARS, MAX_FILE_BYTES, MAX_PEERS, PROTOCOL_VERSION,
};
use silent_core::{
    fingerprint_from_sdp, InviteCode, Opener, PairHandshake, PairKeys, PeerId, SealError, Sealer,
};
use tokio::sync::{mpsc, oneshot};

use crate::media::{
    apply_encoding, hardware_encoder, stereo_computer_audio, test_pattern, AudioInput,
    CameraProfile, CameraTarget, CodecChoice, LocalMedia, Producer, ScreenAudioInput,
    ScreenProfile, ScreenTarget, VideoInput, VideoKind, STREAM_AUDIO, STREAM_SCREEN_AUDIO,
};
use crate::signaling::{self, SignalError, SignalLink, Welcome};
use crate::stats::{self, PeerStats, Quality};

const START_BITRATE_BPS: u64 = 1_200_000;
/// The loudest [`Room::set_friend_volume`] allows: 200 %.
pub const MAX_FRIEND_VOLUME: f64 = 2.0;
/// A connection that reports "disconnected" often recovers by itself within
/// a few seconds; only then is ICE restarted.
const DISCONNECT_GRACE: Duration = Duration::from_secs(3);
const RESTART_EVERY: Duration = Duration::from_secs(10);
const MAX_RESTARTS: u32 = 12;
/// How long a friend who left the relay keeps their direct connection while
/// it is not working, before they count as gone.
const ORPHAN_GRACE: Duration = Duration::from_secs(10);
/// How long an off-relay friend whose data channel closed has to come back
/// on the relay before they count as gone. When the relay returns, the
/// friend often verifies the new pair first and closes the old connection,
/// which arrives directly, before their Hello arrives through the relay.
const HANGUP_GRACE: Duration = Duration::from_secs(5);
const RELAY_RETRY_MAX: Duration = Duration::from_secs(15);
/// Unsent bytes a data channel may hold before a file transfer waits.
const FILE_BUFFER_HIGH: u64 = 4 * 1024 * 1024;
/// Files one friend may be sending at once.
const MAX_INCOMING_FILES: usize = 4;
/// How often to ask the relay for fresh TURN credentials (they last four
/// hours), so a long call can still reconnect through TURN.
const ICE_REFRESH_EVERY: Duration = Duration::from_secs(3 * 60 * 60);
/// How often audio levels are read for the speaking indicators.
const LEVELS_EVERY: Duration = Duration::from_millis(250);
/// Audio level (0..1, linear) above which someone counts as speaking.
const SPEAKING_LEVEL: f64 = 0.02;
/// How long someone stays "speaking" after the last loud moment, so the
/// indicator doesn't flicker between words.
const SPEAKING_HOLD: Duration = Duration::from_millis(600);

#[derive(Debug, Clone)]
pub struct Settings {
    pub relay_url: String,
    pub name: String,
    pub audio: AudioInput,
    pub camera: CameraProfile,
    pub screen: ScreenProfile,
    /// Route all media through TURN so friends never learn each other's IP.
    pub relay_only: bool,
}

#[derive(Debug, Clone)]
pub enum RoomEvent {
    /// The relay announced someone; key exchange is starting.
    PeerJoining {
        peer: PeerId,
    },
    /// They typed the same code. Their name is now known.
    PeerVerified {
        peer: PeerId,
        name: String,
        /// Set when this is someone already in the call, back under a new id
        /// after reconnecting to the relay. The old id is gone; no
        /// [`RoomEvent::PeerLeft`] is sent for it.
        replaces: Option<PeerId>,
    },
    /// They typed a different code. Nothing further is exchanged with them.
    PeerWrongCode {
        peer: PeerId,
    },
    /// Direct encrypted connection is up. Sent again after a reconnect.
    PeerConnected {
        peer: PeerId,
        safety_words: [&'static str; 4],
    },
    /// Media stopped flowing. The room is trying to reconnect; expect
    /// [`RoomEvent::PeerConnected`] or [`RoomEvent::PeerLeft`] next.
    PeerConnectionLost {
        peer: PeerId,
    },
    /// Gone, whether or not they were verified. Every
    /// [`RoomEvent::PeerJoining`] ends in this, a
    /// [`RoomEvent::PeerVerified`] that replaces them, or the room closing.
    PeerLeft {
        peer: PeerId,
    },
    PeerMedia {
        peer: PeerId,
        state: MediaState,
    },
    Chat {
        from: PeerId,
        text: String,
        sent_at_ms: u64,
    },
    RemoteVideo {
        peer: PeerId,
        kind: VideoKind,
        track: RtcVideoTrack,
    },
    LocalMediaError {
        kind: VideoKind,
        message: String,
    },
    /// Who is talking right now, yourself included as [`Room::me`]. Sent
    /// whenever the set changes.
    Speaking {
        peers: Vec<PeerId>,
    },
    /// A friend's connection got poor (lost audio, long delay) or good
    /// again. Every friend starts out good.
    ConnectionQuality {
        peer: PeerId,
        quality: Quality,
    },
    /// A friend started sending a file. `id` is theirs, unique per sender.
    FileIncoming {
        from: PeerId,
        id: u32,
        name: String,
        size: u64,
    },
    /// The whole file arrived. It lives only in this event.
    FileReceived {
        from: PeerId,
        id: u32,
        name: String,
        data: Vec<u8>,
    },
    /// A transfer stopped before the end (they cancelled or left).
    FileFailed {
        from: PeerId,
        id: u32,
    },
    /// The relay connection dropped. Calls keep going peer to peer and the
    /// room keeps trying to get back on the relay; until then nobody new can
    /// join.
    RelayLost,
    RelayRestored,
    /// Everyone who was in the room before you had you removed: you typed a
    /// different code than they did. The room is over for you.
    Removed,
}

enum Command {
    Chat(String),
    Mic(bool),
    Camera(Option<CameraTarget>),
    Screen(Option<ScreenTarget>),
    Restart,
    MuteFriend(PeerId, bool),
    FriendVolume(PeerId, f64),
    SendFile(String, Arc<[u8]>),
    Stats(oneshot::Sender<Vec<PeerStats>>),
    Leave(oneshot::Sender<()>),
}

enum Internal {
    Ice {
        peer: PeerId,
        mid: String,
        mline: i32,
        candidate: String,
    },
    PcState {
        peer: PeerId,
        state: PeerConnectionState,
    },
    Track {
        peer: PeerId,
        kind: Option<VideoKind>,
        /// An audio track carrying the friend's computer audio.
        screen_audio: bool,
        track: MediaStreamTrack,
    },
    DcOpen {
        peer: PeerId,
    },
    DcMessage {
        peer: PeerId,
        data: Vec<u8>,
    },
    DcClosed {
        peer: PeerId,
    },
    /// `HANGUP_GRACE` after an off-relay friend's data channel closed.
    HungUp {
        peer: PeerId,
    },
    FileData {
        peer: PeerId,
        data: Vec<u8>,
    },
    CaptureFailed {
        kind: VideoKind,
        message: String,
    },
    /// A scheduled check on a connection that isn't working. Stale when
    /// `generation` no longer matches the peer's.
    Recover {
        peer: PeerId,
        generation: u64,
    },
    RelayBack {
        welcome: Welcome,
        link: SignalLink,
    },
    Levels {
        me: Option<f64>,
        peers: Vec<(PeerId, stats::Pulse)>,
    },
}

/// Handle to a running room. Dropping it leaves the room.
pub struct Room {
    code: InviteCode,
    me: PeerId,
    commands: mpsc::UnboundedSender<Command>,
    events: Option<mpsc::UnboundedReceiver<RoomEvent>>,
    local_camera: RtcVideoTrack,
    local_screen: RtcVideoTrack,
    camera_input: VideoInput,
    screen_input: VideoInput,
    screen_audio_input: ScreenAudioInput,
    platform_audio: bool,
}

impl Room {
    pub(crate) fn start(
        factory: PeerConnectionFactory,
        settings: Settings,
        code: InviteCode,
        welcome: Welcome,
        link: SignalLink,
    ) -> Self {
        let media = LocalMedia::new(&factory, settings.audio, settings.camera, settings.screen);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (int_tx, int_rx) = mpsc::unbounded_channel();

        let room = Room {
            code: code.clone(),
            me: welcome.me,
            commands: cmd_tx,
            events: Some(ev_rx),
            local_camera: media.camera.track.clone(),
            local_screen: media.screen.track.clone(),
            camera_input: VideoInput {
                source: media.camera.source.clone(),
                encoding: media.camera.encoding,
            },
            screen_input: VideoInput {
                source: media.screen.source.clone(),
                encoding: media.screen.encoding,
            },
            screen_audio_input: media.screen_audio_input.clone(),
            platform_audio: media.has_platform_audio(),
        };

        let actor = Actor {
            code,
            me: welcome.me,
            relay_url: settings.relay_url.clone(),
            relay_only: settings.relay_only,
            relay_me: welcome.me,
            session: new_session_id(),
            name: clean_name(&settings.name),
            factory,
            rtc_config: rtc_config(&welcome.ice, settings.relay_only),
            codec: CodecChoice::detect(),
            media,
            media_state: MediaState {
                mic: true,
                camera: false,
                screen: false,
            },
            peers: HashMap::new(),
            ids: Ids::new(welcome.me),
            relay: Some(link.outbound),
            loud_at: HashMap::new(),
            next_file_id: 0,
            speaking: Vec::new(),
            levels_pending: false,
            evicted: false,
            events: ev_tx,
            internal: int_tx,
        };
        tokio::spawn(actor.run(welcome.peers, link.inbound, cmd_rx, int_rx));
        room
    }

    pub fn code(&self) -> &InviteCode {
        &self.code
    }

    /// Your id in this room. It is the relay id you were given on joining,
    /// so in a room that never reconnected it matches the id friends see for
    /// you; friends' ids in events never collide with it.
    pub fn me(&self) -> PeerId {
        self.me
    }

    /// Takes the event stream. Call once.
    pub fn take_events(&mut self) -> mpsc::UnboundedReceiver<RoomEvent> {
        self.events.take().expect("events already taken")
    }

    /// Where to push frames after `set_camera(Some(CameraTarget::External))`
    /// or `share_screen(Some(ScreenTarget::External))`.
    pub fn video_input(&self, kind: VideoKind) -> VideoInput {
        match kind {
            VideoKind::Camera => self.camera_input.clone(),
            VideoKind::Screen => self.screen_input.clone(),
        }
    }

    /// Where to push the computer's sound while sharing the screen.
    pub fn screen_audio_input(&self) -> ScreenAudioInput {
        self.screen_audio_input.clone()
    }

    pub fn local_video(&self, kind: VideoKind) -> RtcVideoTrack {
        match kind {
            VideoKind::Camera => self.local_camera.clone(),
            VideoKind::Screen => self.local_screen.clone(),
        }
    }

    /// False when no audio device could be opened and silence is being sent.
    pub fn has_microphone(&self) -> bool {
        self.platform_audio
    }

    pub fn send_chat(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Chat(text.into()));
    }

    pub fn set_mic(&self, on: bool) {
        let _ = self.commands.send(Command::Mic(on));
    }

    pub fn set_camera(&self, target: Option<CameraTarget>) {
        let _ = self.commands.send(Command::Camera(target));
    }

    pub fn share_screen(&self, target: Option<ScreenTarget>) {
        let _ = self.commands.send(Command::Screen(target));
    }

    /// Sends a file to every friend in the call, over the encrypted data
    /// channels. Fails only when it is larger than [`MAX_FILE_BYTES`].
    pub fn send_file(&self, name: &str, data: Vec<u8>) -> Result<(), FileTooLarge> {
        if data.len() as u64 > MAX_FILE_BYTES {
            return Err(FileTooLarge);
        }
        let _ = self
            .commands
            .send(Command::SendFile(clean_file_name(name), data.into()));
        Ok(())
    }

    /// Silences a friend for you only (they aren't told), or unmutes them.
    pub fn set_friend_muted(&self, peer: PeerId, muted: bool) {
        let _ = self.commands.send(Command::MuteFriend(peer, muted));
    }

    /// Sets how loud a friend plays for you only: 1.0 is normal, 0.0 silent,
    /// up to [`MAX_FRIEND_VOLUME`] (louder clips). Kept if they reconnect.
    pub fn set_friend_volume(&self, peer: PeerId, volume: f64) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, MAX_FRIEND_VOLUME)
        } else {
            1.0
        };
        let _ = self.commands.send(Command::FriendVolume(peer, volume));
    }

    /// Restarts ICE on every connection. Call it when the device switched
    /// networks (Wi-Fi to mobile data, say) so calls move over right away
    /// instead of after they time out.
    pub fn restart_connections(&self) {
        let _ = self.commands.send(Command::Restart);
    }

    /// Video statistics for every connected friend. The request is sent
    /// immediately; the returned future does not borrow the room.
    pub fn stats(&self) -> impl std::future::Future<Output = Vec<PeerStats>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let sent = self.commands.send(Command::Stats(tx)).is_ok();
        async move {
            if sent {
                rx.await.unwrap_or_default()
            } else {
                Vec::new()
            }
        }
    }

    pub async fn leave(self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Leave(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("files can be up to {} MB", MAX_FILE_BYTES / 1024 / 1024)]
pub struct FileTooLarge;

fn rtc_config(ice: &IceConfig, relay_only: bool) -> RtcConfiguration {
    let mut config = RtcConfiguration::default();
    config.ice_servers = ice
        .ice_servers
        .iter()
        .map(|s| IceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            password: s.credential.clone().unwrap_or_default(),
        })
        .collect();
    config.continual_gathering_policy = ContinualGatheringPolicy::GatherContinually;
    config.ice_transport_type = if relay_only {
        IceTransportsType::Relay
    } else {
        IceTransportsType::All
    };
    config
}

/// Gets back on the relay under the same room number, recreating the room
/// there if everyone else dropped off too.
async fn rejoin(url: &str, nameplate: u16) -> Result<(Welcome, SignalLink), SignalError> {
    match signaling::connect(url, nameplate, Intent::Join).await {
        Err(SignalError::Relay(RelayErrorCode::NoRoom)) => {
            signaling::connect(url, nameplate, Intent::Create).await
        }
        other => other,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct Peer {
    /// Their current id on the relay; `None` once we can no longer signal
    /// them (see [`Actor::orphan`]).
    relay_id: Option<PeerId>,
    session: Option<String>,
    /// The codec they asked to send (see `Signal::Hello::send_h264`).
    send_codec: Option<CodecChoice>,
    handshake: Option<PairHandshake>,
    keys: Option<PairKeys>,
    sealer: Option<Sealer>,
    opener: Option<Opener>,
    verified: bool,
    rejected: bool,
    pc: Option<PeerConnection>,
    dc: Option<DataChannel>,
    files: Option<DataChannel>,
    incoming: HashMap<u32, Incoming>,
    /// Limits our transfers to them to what they accept at once.
    sending: Option<Arc<tokio::sync::Semaphore>>,
    remote_described: bool,
    pending_ice: Vec<(String, i32, String)>,
    remote_fingerprint: Option<String>,
    announced_connected: bool,
    safety_words: Option<[&'static str; 4]>,
    /// Was connected, and media isn't flowing right now.
    lost: bool,
    restarts: u32,
    recover_generation: u64,
    remote_audio: Option<RtcAudioTrack>,
    /// Their computer audio, sent with a screen share.
    remote_screen_audio: Option<RtcAudioTrack>,
    muted_for_me: bool,
    /// Playback volume set with [`Room::set_friend_volume`]; `None` is normal.
    volume: Option<f64>,
    quality: stats::QualityMeter,
    video_senders: Vec<(VideoKind, RtpSender)>,
}

/// Maps relay ids to the ids used in events. Relay ids can come back for a
/// different person (after the relay restarts, or once a Worker room
/// hibernates), and a friend who rejoins the relay gets a new one, so events
/// use ids that are unique for the life of the room: the relay id when it
/// hasn't been seen before, otherwise a spare one.
struct Ids {
    by_relay: HashMap<PeerId, PeerId>,
    used: HashSet<PeerId>,
    spare: PeerId,
}

impl Ids {
    fn new(me: PeerId) -> Self {
        Self {
            by_relay: HashMap::new(),
            used: HashSet::from([me]),
            spare: 1 << 31,
        }
    }

    fn get(&self, relay_id: PeerId) -> Option<PeerId> {
        self.by_relay.get(&relay_id).copied()
    }

    fn assign(&mut self, relay_id: PeerId) -> PeerId {
        let mut id = relay_id;
        while !self.used.insert(id) {
            self.spare = self.spare.wrapping_add(1);
            id = self.spare;
        }
        self.by_relay.insert(relay_id, id);
        id
    }

    fn forget(&mut self, relay_id: PeerId) -> Option<PeerId> {
        self.by_relay.remove(&relay_id)
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        self.stop_everything();
    }
}

/// A file on its way in.
struct Incoming {
    name: String,
    size: u64,
    data: Vec<u8>,
}

impl Peer {
    /// Applies Mute for me and the volume to their voice and computer audio.
    fn apply_audio(&self) {
        for audio in [&self.remote_audio, &self.remote_screen_audio]
            .into_iter()
            .flatten()
        {
            audio.set_enabled(!self.muted_for_me);
            audio.set_volume(self.volume.unwrap_or(1.0));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        retire(self.pc.take(), [self.dc.take(), self.files.take()]);
    }
}

/// How long a closed connection is kept before its handles are released.
const RETIRE_AFTER: Duration = Duration::from_secs(10);

type Retired = (Instant, Option<PeerConnection>, [Option<DataChannel>; 2]);

/// Closes a connection and parks its handles for a while instead of dropping
/// them. libwebrtc can still deliver queued callbacks (ICE state changes)
/// after `close()`, and webrtc-sys frees the C++ observer as soon as the last
/// Rust handle goes away, which turns those callbacks into use-after-free
/// crashes. Seen as intermittent SIGSEGVs whenever a friend leaves.
fn retire(pc: Option<PeerConnection>, dcs: [Option<DataChannel>; 2]) {
    static GRAVEYARD: Mutex<Vec<Retired>> = Mutex::new(Vec::new());
    if pc.is_none() && dcs.iter().all(Option::is_none) {
        return;
    }
    for dc in dcs.iter().flatten() {
        dc.on_message(None);
        dc.on_state_change(None);
        dc.close();
    }
    if let Some(pc) = &pc {
        pc.on_ice_candidate(None);
        pc.on_connection_state_change(None);
        pc.on_track(None);
        pc.close();
    }
    let now = Instant::now();
    let mut graveyard = GRAVEYARD.lock().unwrap_or_else(|e| e.into_inner());
    graveyard.retain(|(at, _, _)| now.duration_since(*at) < RETIRE_AFTER);
    graveyard.push((now, pc, dcs));
}

struct Actor {
    code: InviteCode,
    relay_url: String,
    relay_only: bool,
    /// Our id in events (see `Room::me`).
    me: PeerId,
    /// Our id on the relay. Changes when the relay connection is re-made.
    relay_me: PeerId,
    session: String,
    name: String,
    factory: PeerConnectionFactory,
    rtc_config: RtcConfiguration,
    codec: CodecChoice,
    media: LocalMedia,
    media_state: MediaState,
    /// Keyed by event id (see [`Ids`]).
    peers: HashMap<PeerId, Peer>,
    ids: Ids,
    relay: Option<mpsc::UnboundedSender<ClientMsg>>,
    /// When each person was last loud; those within `SPEAKING_HOLD` are
    /// speaking.
    loud_at: HashMap<PeerId, Instant>,
    next_file_id: u32,
    speaking: Vec<PeerId>,
    levels_pending: bool,
    /// The relay removed us (see `RoomEvent::Removed`); don't rejoin.
    evicted: bool,
    events: mpsc::UnboundedSender<RoomEvent>,
    internal: mpsc::UnboundedSender<Internal>,
}

impl Actor {
    async fn run(
        mut self,
        initial: Vec<PeerId>,
        mut relay_in: mpsc::UnboundedReceiver<ServerMsg>,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
    ) {
        for peer in initial {
            self.begin_pair(peer);
        }
        let mut relay_open = true;
        let mut levels = tokio::time::interval(LEVELS_EVERY);
        levels.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ice_refresh = tokio::time::interval_at(
            tokio::time::Instant::now() + ICE_REFRESH_EVERY,
            ICE_REFRESH_EVERY,
        );
        loop {
            tokio::select! {
                _ = levels.tick() => self.read_levels(),
                _ = ice_refresh.tick() => {
                    if let Some(relay) = &self.relay {
                        let _ = relay.send(ClientMsg::RefreshIce);
                    }
                }
                msg = relay_in.recv(), if relay_open => match msg {
                    Some(msg) => self.on_relay(msg).await,
                    None => {
                        relay_open = false;
                        self.relay_lost();
                    }
                },
                cmd = commands.recv() => match cmd {
                    Some(Command::Leave(done)) => {
                        self.shutdown();
                        let _ = done.send(());
                        return;
                    }
                    Some(Command::Stats(reply)) => {
                        let pcs: Vec<(PeerId, PeerConnection)> = self
                            .peers
                            .iter()
                            .filter(|(_, p)| p.announced_connected)
                            .filter_map(|(&id, p)| Some((id, p.pc.clone()?)))
                            .collect();
                        tokio::spawn(async move {
                            let mut all = Vec::with_capacity(pcs.len());
                            for (id, pc) in pcs {
                                all.push(stats::collect(id, &pc).await);
                            }
                            let _ = reply.send(all);
                        });
                    }
                    Some(cmd) => self.on_command(cmd).await,
                    None => {
                        self.shutdown();
                        return;
                    }
                },
                Some(ev) = internal.recv() => match ev {
                    Internal::RelayBack { welcome, link } => {
                        relay_in = link.inbound;
                        relay_open = true;
                        self.relay_restored(welcome, link.outbound);
                    }
                    ev => self.on_internal(ev).await,
                },
            }
        }
    }

    fn emit(&self, event: RoomEvent) {
        let _ = self.events.send(event);
    }

    fn relay_frame(&self, relay_id: PeerId, frame: PeerFrame) {
        if let Some(relay) = &self.relay {
            let _ = relay.send(ClientMsg::Relay {
                to: relay_id,
                data: frame.encode(),
            });
        }
    }

    fn send_signal(&mut self, to: PeerId, signal: &Signal) {
        let Some(p) = self.peers.get_mut(&to) else {
            return;
        };
        let (Some(relay_id), Some(sealer)) = (p.relay_id, p.sealer.as_mut()) else {
            return;
        };
        let plaintext = serde_json::to_vec(signal).expect("serializable");
        let sealed = sealer.seal(&plaintext);
        self.relay_frame(relay_id, PeerFrame::Sealed(sealed));
    }

    /// Whether we send the offers to this friend: the higher relay id does.
    fn is_offerer(&self, peer: PeerId) -> bool {
        self.peers
            .get(&peer)
            .and_then(|p| p.relay_id)
            .is_some_and(|r| self.relay_me > r)
    }

    /// Starts pairing with someone on the relay, or returns the event id of
    /// the pairing already under way.
    fn begin_pair(&mut self, relay_id: PeerId) -> Option<PeerId> {
        if relay_id == self.relay_me {
            return None;
        }
        if let Some(id) = self.ids.get(relay_id) {
            return Some(id);
        }
        // A room never has more than MAX_PEERS on the relay; a relay that
        // says otherwise doesn't get us to open connection after connection.
        let on_relay = self.peers.values().filter(|p| p.relay_id.is_some()).count();
        if on_relay + 1 >= MAX_PEERS {
            return None;
        }
        let id = self.ids.assign(relay_id);
        let (handshake, outbound) = PairHandshake::start(&self.code, self.relay_me, relay_id);
        let mut new_peer = Peer::default();
        new_peer.relay_id = Some(relay_id);
        new_peer.handshake = Some(handshake);
        self.peers.insert(id, new_peer);
        self.relay_frame(relay_id, PeerFrame::Pake(outbound));
        self.emit(RoomEvent::PeerJoining { peer: id });
        Some(id)
    }

    async fn on_relay(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Joined { peer } => {
                if let Some(stale) = self.ids.forget(peer) {
                    self.orphan(stale);
                }
                self.begin_pair(peer);
            }
            ServerMsg::Left { peer } => {
                if let Some(id) = self.ids.forget(peer) {
                    self.orphan(id);
                }
            }
            ServerMsg::Relay { from, data } => match PeerFrame::decode(&data) {
                Ok(PeerFrame::Pake(msg)) => {
                    if let Some(id) = self.begin_pair(from) {
                        self.on_pake(id, &msg);
                    }
                }
                Ok(PeerFrame::Sealed(sealed)) => {
                    if let Some(id) = self.ids.get(from) {
                        self.on_sealed(id, &sealed).await;
                    }
                }
                Err(_) => tracing::debug!(peer = from, "ignoring malformed frame"),
            },
            ServerMsg::Error {
                code: RelayErrorCode::Evicted,
            } => self.evicted = true,
            ServerMsg::Ice { ice } => {
                self.rtc_config = rtc_config(&ice, self.relay_only);
                for pc in self.peers.values().filter_map(|p| p.pc.as_ref()) {
                    if let Err(e) = pc.set_configuration(self.rtc_config.clone()) {
                        tracing::debug!("could not update ice servers: {:?}", e.error_type);
                    }
                }
            }
            ServerMsg::Welcome { .. } | ServerMsg::Error { .. } => {}
        }
    }

    /// We can no longer signal this friend: they left the relay, or we lost
    /// it. A working call keeps going over its direct connection until they
    /// are back under a new relay id (or it stops working); anything else is
    /// dropped.
    fn orphan(&mut self, peer: PeerId) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        if let Some(relay_id) = p.relay_id.take() {
            self.ids.forget(relay_id);
        }
        if !p.announced_connected || p.lost {
            self.drop_peer(peer);
        }
    }

    fn relay_lost(&mut self) {
        self.relay = None;
        if self.evicted {
            // Whoever was on the relay first had us removed. Friends we are
            // already in a call with stay, peer to peer; there is just no
            // getting back on the relay.
            let peers: Vec<PeerId> = self.peers.keys().copied().collect();
            for peer in peers {
                self.orphan(peer);
            }
            if self.peers.values().any(|p| p.verified) {
                self.emit(RoomEvent::RelayLost);
            } else {
                self.emit(RoomEvent::Removed);
            }
            return;
        }
        self.emit(RoomEvent::RelayLost);
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            self.orphan(peer);
        }
        let (url, nameplate) = (self.relay_url.clone(), self.code.nameplate());
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            while !internal.is_closed() {
                tokio::time::sleep(delay).await;
                match rejoin(&url, nameplate).await {
                    Ok((welcome, link)) => {
                        let _ = internal.send(Internal::RelayBack { welcome, link });
                        return;
                    }
                    // Often our own old seat, until the relay notices it is dead.
                    Err(e) => tracing::debug!("relay still unreachable: {e}"),
                }
                delay = (delay * 2).min(RELAY_RETRY_MAX);
            }
        });
    }

    fn relay_restored(&mut self, welcome: Welcome, outbound: mpsc::UnboundedSender<ClientMsg>) {
        self.relay = Some(outbound);
        self.relay_me = welcome.me;
        self.rtc_config = rtc_config(&welcome.ice, self.relay_only);
        self.emit(RoomEvent::RelayRestored);
        for peer in welcome.peers {
            self.begin_pair(peer);
        }
    }

    fn on_pake(&mut self, from: PeerId, msg: &[u8]) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        let Some(handshake) = peer.handshake.take() else {
            return;
        };
        let keys = match handshake.finish(msg) {
            Ok(keys) => keys,
            Err(_) => {
                peer.rejected = true;
                self.vote_evict(from);
                self.emit(RoomEvent::PeerWrongCode { peer: from });
                return;
            }
        };
        let (sealer, opener) = keys.split();
        peer.sealer = Some(sealer);
        peer.opener = Some(opener);
        peer.keys = Some(keys);

        if let Err(e) = self.open_connection(from) {
            tracing::warn!(peer = from, "could not create peer connection: {e}");
            return;
        }
        let hello = Signal::Hello {
            version: PROTOCOL_VERSION,
            name: self.name.clone(),
            session: self.session.clone(),
            send_h264: self.codec == CodecChoice::H264,
        };
        self.send_signal(from, &hello);
    }

    async fn on_sealed(&mut self, from: PeerId, sealed: &[u8]) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        if peer.rejected {
            return;
        }
        let Some(opener) = peer.opener.as_mut() else {
            return;
        };
        let plaintext = match opener.open(sealed) {
            Ok(p) => p,
            Err(e) => {
                if peer.verified {
                    tracing::warn!(peer = from, "dropping peer after bad envelope: {e}");
                    self.drop_peer(from);
                } else {
                    // Only a message that fails authentication means a
                    // different code; a gap in the counter is a lost message.
                    let forged = !matches!(e, SealError::OutOfOrder { .. });
                    self.reject(from, forged);
                }
                return;
            }
        };
        let Ok(signal) = serde_json::from_slice::<Signal>(&plaintext) else {
            tracing::debug!(peer = from, "ignoring undecodable signal");
            return;
        };
        // The error can quote SDP or ICE lines, which hold IP addresses.
        if self.on_signal(from, signal).await.is_err() {
            tracing::warn!(peer = from, "negotiation failed");
        }
    }

    fn reject(&mut self, peer: PeerId, vote: bool) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.rejected = true;
            retire(p.pc.take(), [p.dc.take(), p.files.take()]);
        }
        if vote {
            self.vote_evict(peer);
        }
        self.emit(RoomEvent::PeerWrongCode { peer });
    }

    /// Asks the relay to remove someone who typed a different code. It only
    /// counts if we were in the room first (see `ClientMsg::Evict`).
    fn vote_evict(&self, peer: PeerId) {
        let relay_id = self.peers.get(&peer).and_then(|p| p.relay_id);
        if let (Some(relay), Some(relay_id)) = (&self.relay, relay_id) {
            let _ = relay.send(ClientMsg::Evict { peer: relay_id });
        }
    }

    async fn on_signal(&mut self, from: PeerId, signal: Signal) -> anyhow::Result<()> {
        match signal {
            Signal::Hello {
                version,
                name,
                session,
                send_h264,
            } => {
                if version != PROTOCOL_VERSION {
                    anyhow::bail!("peer speaks protocol v{version}");
                }
                let Some(peer) = self.peers.get_mut(&from) else {
                    return Ok(());
                };
                if peer.verified {
                    return Ok(());
                }
                peer.verified = true;
                peer.session = Some(session.clone());
                peer.send_codec = Some(if send_h264 {
                    CodecChoice::H264
                } else {
                    CodecChoice::Default
                });
                let replaces = self
                    .peers
                    .iter()
                    .find(|(&id, p)| id != from && p.session.as_deref() == Some(session.as_str()))
                    .map(|(&id, _)| id);
                if let Some(old) = replaces.and_then(|old| self.peers.remove(&old)) {
                    self.fail_incoming(replaces.expect("replaced"), &old);
                    // Retired by `Peer`'s drop.
                    if let Some(relay_id) = old.relay_id {
                        self.ids.forget(relay_id);
                    }
                    if let Some(p) = self.peers.get_mut(&from) {
                        p.muted_for_me = old.muted_for_me;
                        p.volume = old.volume;
                        p.apply_audio();
                    }
                }
                // One more friend: cameras shrink for everyone (`mesh_scale`).
                self.sync_senders(None);
                self.emit(RoomEvent::PeerVerified {
                    peer: from,
                    name: clean_name(&name),
                    replaces,
                });
                if self.is_offerer(from) {
                    self.send_offer(from, false).await?;
                }
            }
            Signal::Offer { sdp } => {
                let pc = self.pc(from)?;
                let offer = SessionDescription::parse(&sdp, SdpType::Offer)?;
                pc.set_remote_description(offer).await?;
                if self
                    .peers
                    .get(&from)
                    .is_some_and(|p| p.video_senders.is_empty())
                {
                    self.add_local_tracks(from, &pc)?;
                }
                self.prefer_codecs(from, &pc);
                let answer = pc.create_answer(AnswerOptions::default()).await?;
                let answer = SessionDescription::parse(
                    &stereo_computer_audio(&answer.to_string()),
                    SdpType::Answer,
                )?;
                pc.set_local_description(answer.clone()).await?;
                self.after_remote_description(from, &sdp).await;
                self.send_signal(
                    from,
                    &Signal::Answer {
                        sdp: answer.to_string(),
                    },
                );
            }
            Signal::Answer { sdp } => {
                let pc = self.pc(from)?;
                // An answer to an offer we already replaced is stale.
                if pc.signaling_state() != SignalingState::HaveLocalOffer {
                    return Ok(());
                }
                let answer = SessionDescription::parse(&sdp, SdpType::Answer)?;
                pc.set_remote_description(answer).await?;
                self.after_remote_description(from, &sdp).await;
            }
            Signal::Ice {
                mid,
                mline,
                candidate,
            } => {
                let Some(peer) = self.peers.get_mut(&from) else {
                    return Ok(());
                };
                if peer.remote_described {
                    let pc = self.pc(from)?;
                    pc.add_ice_candidate(IceCandidate::parse(&mid, mline, &candidate)?)
                        .await?;
                } else {
                    peer.pending_ice.push((mid, mline, candidate));
                }
            }
            Signal::Restart => {
                // While our own restart offer is out, it already answers this.
                let idle = self
                    .pc(from)
                    .is_ok_and(|pc| pc.signaling_state() == SignalingState::Stable);
                if self.is_offerer(from) && idle {
                    self.send_offer(from, true).await?;
                }
            }
            Signal::Bye => self.drop_peer(from),
        }
        Ok(())
    }

    fn pc(&self, peer: PeerId) -> anyhow::Result<PeerConnection> {
        self.peers
            .get(&peer)
            .and_then(|p| p.pc.clone())
            .ok_or_else(|| anyhow::anyhow!("no connection for peer {peer}"))
    }

    async fn send_offer(&mut self, peer: PeerId, ice_restart: bool) -> anyhow::Result<()> {
        let pc = self.pc(peer)?;
        if ice_restart {
            // Picks up fresh TURN credentials if the relay handed out new ones.
            if let Err(e) = pc.set_configuration(self.rtc_config.clone()) {
                tracing::debug!(peer, "could not update ice servers: {:?}", e.error_type);
            }
        }
        self.prefer_codecs(peer, &pc);
        let options = OfferOptions {
            ice_restart,
            offer_to_receive_audio: true,
            offer_to_receive_video: true,
        };
        let offer = pc.create_offer(options).await?;
        let offer =
            SessionDescription::parse(&stereo_computer_audio(&offer.to_string()), SdpType::Offer)?;
        pc.set_local_description(offer.clone()).await?;
        self.send_signal(
            peer,
            &Signal::Offer {
                sdp: offer.to_string(),
            },
        );
        Ok(())
    }

    /// Restarts ICE with one friend, or asks them to when they are the
    /// offering side.
    async fn restart(&mut self, peer: PeerId) {
        if self.is_offerer(peer) {
            if let Err(e) = self.send_offer(peer, true).await {
                tracing::debug!(peer, "ice restart failed: {e}");
            }
        } else {
            self.send_signal(peer, &Signal::Restart);
        }
    }

    async fn after_remote_description(&mut self, peer: PeerId, remote_sdp: &str) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.remote_described = true;
        p.remote_fingerprint = fingerprint_from_sdp(remote_sdp);
        let pending = std::mem::take(&mut p.pending_ice);
        let Some(pc) = p.pc.clone() else { return };
        for (mid, mline, candidate) in pending {
            match IceCandidate::parse(&mid, mline, &candidate) {
                Ok(c) => {
                    if let Err(e) = pc.add_ice_candidate(c).await {
                        tracing::debug!(peer, "ice candidate rejected: {:?}", e.error_type);
                    }
                }
                Err(_) => tracing::debug!(peer, "bad ice candidate"),
            }
        }
    }

    fn open_connection(&mut self, peer: PeerId) -> anyhow::Result<()> {
        let pc = self
            .factory
            .create_peer_connection(self.rtc_config.clone())?;
        // Start the bandwidth estimate where 720p looks decent instead of
        // WebRTC's conservative default, so video sharpens within seconds.
        let bitrate = BitrateSettings {
            min_bitrate_bps: None,
            start_bitrate_bps: Some(START_BITRATE_BPS),
            max_bitrate_bps: None,
        };
        if let Err(e) = pc.set_bitrate(bitrate) {
            tracing::debug!(peer, "could not set start bitrate: {e}");
        }

        let tx = self.internal.clone();
        pc.on_ice_candidate(Some(Box::new(move |c: IceCandidate| {
            let _ = tx.send(Internal::Ice {
                peer,
                mid: c.sdp_mid(),
                mline: c.sdp_mline_index(),
                candidate: c.candidate(),
            });
        })));
        let tx = self.internal.clone();
        pc.on_connection_state_change(Some(Box::new(move |state| {
            let _ = tx.send(Internal::PcState { peer, state });
        })));
        let tx = self.internal.clone();
        pc.on_track(Some(Box::new(move |ev| {
            let streams: Vec<String> = ev
                .streams
                .iter()
                .map(|s| s.id())
                .chain(ev.receiver.stream_ids())
                .collect();
            let kind = streams.iter().find_map(|id| VideoKind::from_stream_id(id));
            let screen_audio = streams.iter().any(|id| id == STREAM_SCREEN_AUDIO);
            tracing::debug!(peer, ?kind, screen_audio, mid = ?ev.transceiver.mid(), "remote track");
            let _ = tx.send(Internal::Track {
                peer,
                kind,
                screen_audio,
                track: ev.track,
            });
        })));

        let dc = pc.create_data_channel(
            "silent",
            DataChannelInit {
                negotiated: true,
                id: CHAT_CHANNEL.into(),
                ordered: true,
                ..Default::default()
            },
        )?;
        let files = pc.create_data_channel(
            "silent-files",
            DataChannelInit {
                negotiated: true,
                id: FILE_CHANNEL.into(),
                ordered: true,
                ..Default::default()
            },
        )?;
        let tx = self.internal.clone();
        files.on_message(Some(Box::new(move |buf: DataBuffer| {
            let _ = tx.send(Internal::FileData {
                peer,
                data: buf.data.to_vec(),
            });
        })));
        let tx = self.internal.clone();
        dc.on_state_change(Some(Box::new(move |state| {
            let _ = match state {
                DataChannelState::Open => tx.send(Internal::DcOpen { peer }),
                DataChannelState::Closed => tx.send(Internal::DcClosed { peer }),
                _ => Ok(()),
            };
        })));
        let tx = self.internal.clone();
        dc.on_message(Some(Box::new(move |buf: DataBuffer| {
            let _ = tx.send(Internal::DcMessage {
                peer,
                data: buf.data.to_vec(),
            });
        })));

        if self.is_offerer(peer) {
            self.add_local_transceivers(peer, &pc)?;
        }

        let p = self.peers.get_mut(&peer).expect("peer exists");
        p.pc = Some(pc);
        p.dc = Some(dc);
        p.files = Some(files);
        Ok(())
    }

    /// The offerer creates send-and-receive transceivers up front (this
    /// binding's `add_track` would make them send-only).
    fn add_local_transceivers(&mut self, peer: PeerId, pc: &PeerConnection) -> anyhow::Result<()> {
        let init = |stream: &str| RtpTransceiverInit {
            direction: RtpTransceiverDirection::SendRecv,
            stream_ids: vec![stream.to_string()],
            send_encodings: Vec::new(),
        };
        pc.add_transceiver(
            MediaStreamTrack::Audio(self.media.audio.clone()),
            init(STREAM_AUDIO),
        )?;
        let mut senders = Vec::new();
        for kind in [VideoKind::Camera, VideoKind::Screen] {
            let video = self.media.video(kind);
            let track = MediaStreamTrack::Video(video.track.clone());
            let sender = pc.add_transceiver(track, init(kind.stream_id()))?.sender();
            senders.push((kind, sender));
        }
        pc.add_transceiver(
            MediaStreamTrack::Audio(self.media.screen_audio.clone()),
            init(STREAM_SCREEN_AUDIO),
        )?;
        self.attach_senders(peer, senders);
        Ok(())
    }

    /// The answerer adds its tracks after applying the offer, so libwebrtc
    /// attaches them to the offer's transceivers (in m-line order) instead of
    /// creating new ones.
    fn add_local_tracks(&mut self, peer: PeerId, pc: &PeerConnection) -> anyhow::Result<()> {
        pc.add_track(
            MediaStreamTrack::Audio(self.media.audio.clone()),
            &[STREAM_AUDIO],
        )?;
        let mut senders = Vec::new();
        for kind in [VideoKind::Camera, VideoKind::Screen] {
            let video = self.media.video(kind);
            let track = MediaStreamTrack::Video(video.track.clone());
            let sender = pc.add_track(track, &[kind.stream_id()])?;
            senders.push((kind, sender));
        }
        pc.add_track(
            MediaStreamTrack::Audio(self.media.screen_audio.clone()),
            &[STREAM_SCREEN_AUDIO],
        )?;
        self.attach_senders(peer, senders);
        Ok(())
    }

    /// Each side sends with the first codec the other side lists, so the
    /// order we list is the codec this friend asked to send.
    fn prefer_codecs(&self, peer: PeerId, pc: &PeerConnection) {
        let choice = self.peers.get(&peer).and_then(|p| p.send_codec);
        let Some(codecs) = choice.and_then(|c| c.preferences(&self.factory)) else {
            return;
        };
        for transceiver in pc.transceivers() {
            if matches!(
                transceiver.receiver().track(),
                Some(MediaStreamTrack::Video(_))
            ) {
                if let Err(e) = transceiver.set_codec_preferences(codecs.clone()) {
                    tracing::debug!(peer, "could not set codec preferences: {e}");
                }
            }
        }
    }

    fn attach_senders(&mut self, peer: PeerId, senders: Vec<(VideoKind, RtpSender)>) {
        if hardware_encoder() {
            for (_, sender) in &senders {
                sender.set_video_encoder_backend(VideoEncoderBackend::Hardware);
            }
        }
        if let Some(p) = self.peers.get_mut(&peer) {
            p.video_senders = senders;
        }
        self.sync_senders(Some(peer));
    }

    /// Video that is switched off has its encoding deactivated, so nothing is
    /// encoded or sent for it (the source would otherwise feed black
    /// keepalive frames to every connection). No renegotiation is needed.
    /// `set_track(None)` would be the obvious alternative but crashes in
    /// libwebrtc-sys 0.3.49 (it dereferences the null track).
    fn sync_senders(&self, only: Option<PeerId>) {
        let friends = self.peers.values().filter(|p| p.verified).count();
        for (&id, p) in &self.peers {
            if only.is_some_and(|o| o != id) {
                continue;
            }
            for (kind, sender) in &p.video_senders {
                let (on, encoding, scale) = match kind {
                    VideoKind::Camera => {
                        let encoding = &self.media.camera.encoding;
                        (
                            self.media_state.camera,
                            encoding,
                            encoding.mesh_scale(friends),
                        )
                    }
                    // Screens stay sharp: text has to be readable.
                    VideoKind::Screen => {
                        (self.media_state.screen, &self.media.screen.encoding, 1.0)
                    }
                };
                apply_encoding(sender, encoding, on, scale);
            }
        }
    }

    fn drop_peer(&mut self, peer: PeerId) {
        // Dropping the peer retires its connection (see `retire`).
        let Some(p) = self.peers.remove(&peer) else {
            return;
        };
        if let Some(relay_id) = p.relay_id {
            self.ids.forget(relay_id);
        }
        if p.verified {
            self.sync_senders(None);
        }
        self.fail_incoming(peer, &p);
        self.emit(RoomEvent::PeerLeft { peer });
    }

    fn fail_incoming(&self, peer: PeerId, p: &Peer) {
        for &id in p.incoming.keys() {
            self.emit(RoomEvent::FileFailed { from: peer, id });
        }
    }

    fn on_file_frame(&mut self, peer: PeerId, data: &[u8]) {
        let Some(p) = self.peers.get_mut(&peer).filter(|p| p.verified) else {
            return;
        };
        let event = match FileFrame::decode(data) {
            Ok(FileFrame::Start { id, name, size }) => {
                let fits = size <= MAX_FILE_BYTES
                    && p.incoming.len() < MAX_INCOMING_FILES
                    && !p.incoming.contains_key(&id);
                if !fits {
                    tracing::debug!(peer, "ignoring a file offer over the limits");
                    return;
                }
                p.incoming.insert(
                    id,
                    Incoming {
                        name: name.clone(),
                        size,
                        data: Vec::with_capacity(size.min(FILE_BUFFER_HIGH) as usize),
                    },
                );
                self.emit(RoomEvent::FileIncoming {
                    from: peer,
                    id,
                    name,
                    size,
                });
                self.finish_file(peer, id)
            }
            Ok(FileFrame::Chunk { id, data }) => {
                let Some(file) = p.incoming.get_mut(&id) else {
                    return;
                };
                if (file.data.len() + data.len()) as u64 > file.size {
                    p.incoming.remove(&id);
                    Some(RoomEvent::FileFailed { from: peer, id })
                } else {
                    file.data.extend_from_slice(&data);
                    self.finish_file(peer, id)
                }
            }
            Ok(FileFrame::Cancel { id }) => p
                .incoming
                .remove(&id)
                .map(|_| RoomEvent::FileFailed { from: peer, id }),
            Err(_) => None,
        };
        if let Some(event) = event {
            self.emit(event);
        }
    }

    /// Hands over a file once all of it is here.
    fn finish_file(&mut self, peer: PeerId, id: u32) -> Option<RoomEvent> {
        let p = self.peers.get_mut(&peer)?;
        if p.incoming.get(&id)?.data.len() as u64 != p.incoming.get(&id)?.size {
            return None;
        }
        let file = p.incoming.remove(&id)?;
        Some(RoomEvent::FileReceived {
            from: peer,
            id,
            name: file.name,
            data: file.data,
        })
    }

    /// Sends a file to every verified friend, each over their own channel and
    /// at the pace their connection takes.
    fn send_file(&mut self, name: String, data: Arc<[u8]>) {
        let id = self.next_file_id;
        self.next_file_id = self.next_file_id.wrapping_add(1);
        for p in self.peers.values_mut().filter(|p| p.verified) {
            let Some(dc) = p.files.clone() else { continue };
            let slots = p
                .sending
                .get_or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MAX_INCOMING_FILES)))
                .clone();
            let (name, data) = (name.clone(), data.clone());
            tokio::spawn(async move {
                let Ok(_slot) = slots.acquire_owned().await else {
                    return;
                };
                let start = FileFrame::Start {
                    id,
                    name,
                    size: data.len() as u64,
                };
                if dc.send(&start.encode(), true).is_err() {
                    return;
                }
                for chunk in data.chunks(FILE_CHUNK) {
                    while dc.buffered_amount() > FILE_BUFFER_HIGH {
                        if dc.state() != DataChannelState::Open {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    let frame = FileFrame::Chunk {
                        id,
                        data: chunk.to_vec(),
                    };
                    if dc.send(&frame.encode(), true).is_err() {
                        return;
                    }
                }
            });
        }
    }

    async fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::Ice {
                peer,
                mid,
                mline,
                candidate,
            } => {
                self.send_signal(
                    peer,
                    &Signal::Ice {
                        mid,
                        mline,
                        candidate,
                    },
                );
            }
            Internal::PcState { peer, state } => self.on_pc_state(peer, state),
            Internal::Track {
                peer,
                kind,
                screen_audio,
                track,
            } => match track {
                MediaStreamTrack::Video(track) => {
                    if let Some(kind) = kind {
                        if self.peers.get(&peer).is_some_and(|p| p.verified) {
                            self.emit(RoomEvent::RemoteVideo { peer, kind, track });
                        }
                    }
                }
                MediaStreamTrack::Audio(track) => {
                    if let Some(p) = self.peers.get_mut(&peer) {
                        if screen_audio {
                            p.remote_screen_audio = Some(track);
                        } else {
                            p.remote_audio = Some(track);
                        }
                        p.apply_audio();
                    }
                }
            },
            Internal::Levels { me, peers } => self.on_levels(me, peers),
            Internal::DcOpen { peer } => {
                let state = self.media_state;
                self.send_channel(peer, &ChannelMsg::Media(state));
            }
            Internal::DcMessage { peer, data } => self.on_channel(peer, &data),
            Internal::FileData { peer, data } => self.on_file_frame(peer, &data),
            Internal::DcClosed { peer } => {
                // An orphan's data channel closing means they hung up, or
                // that they are back on the relay under a new id: give the
                // new pair time to replace this one.
                if self.peers.get(&peer).is_some_and(|p| p.relay_id.is_none()) {
                    let internal = self.internal.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(HANGUP_GRACE).await;
                        let _ = internal.send(Internal::HungUp { peer });
                    });
                }
            }
            Internal::HungUp { peer } => {
                // Still here, so nobody replaced them.
                if self.peers.get(&peer).is_some_and(|p| p.relay_id.is_none()) {
                    self.drop_peer(peer);
                }
            }
            Internal::CaptureFailed { kind, message } => {
                self.media.video(kind).producer = None;
                match kind {
                    VideoKind::Camera => self.media_state.camera = false,
                    VideoKind::Screen => self.media_state.screen = false,
                }
                self.sync_senders(None);
                self.broadcast_media();
                self.emit(RoomEvent::LocalMediaError { kind, message });
            }
            Internal::Recover { peer, generation } => self.recover(peer, generation).await,
            Internal::RelayBack { .. } => unreachable!("handled in run loop"),
        }
    }

    fn on_pc_state(&mut self, peer: PeerId, state: PeerConnectionState) {
        tracing::debug!(me = self.relay_me, peer, ?state, "connection state");
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        match state {
            PeerConnectionState::Connected => {
                p.restarts = 0;
                p.recover_generation += 1;
                if !p.announced_connected {
                    p.announced_connected = true;
                    let local =
                        p.pc.as_ref()
                            .and_then(|pc| pc.current_local_description())
                            .and_then(|d| fingerprint_from_sdp(&d.to_string()));
                    let words = match (&p.keys, local, &p.remote_fingerprint) {
                        (Some(keys), Some(local), Some(remote)) => {
                            keys.safety_words(&local, remote)
                        }
                        _ => ["?", "?", "?", "?"],
                    };
                    p.safety_words = Some(words);
                } else if !p.lost {
                    return;
                }
                p.lost = false;
                let safety_words = p.safety_words.unwrap_or(["?"; 4]);
                self.emit(RoomEvent::PeerConnected { peer, safety_words });
            }
            PeerConnectionState::Disconnected | PeerConnectionState::Failed => {
                if p.announced_connected && !p.lost {
                    p.lost = true;
                    self.emit(RoomEvent::PeerConnectionLost { peer });
                }
                let p = self.peers.get_mut(&peer).expect("peer exists");
                let orphan = p.relay_id.is_none();
                if orphan && state == PeerConnectionState::Failed {
                    self.drop_peer(peer);
                    return;
                }
                let wait = match (orphan, state) {
                    (true, _) => ORPHAN_GRACE,
                    (false, PeerConnectionState::Failed) => Duration::ZERO,
                    (false, _) => DISCONNECT_GRACE,
                };
                self.schedule_recover(peer, wait);
            }
            _ => {}
        }
    }

    /// Checks on a connection that isn't working after `wait`. Any later
    /// state change makes the check stale.
    fn schedule_recover(&mut self, peer: PeerId, wait: Duration) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.recover_generation += 1;
        let generation = p.recover_generation;
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = internal.send(Internal::Recover { peer, generation });
        });
    }

    async fn recover(&mut self, peer: PeerId, generation: u64) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        let working =
            p.pc.as_ref()
                .is_some_and(|pc| pc.connection_state() == PeerConnectionState::Connected);
        if p.recover_generation != generation || working {
            return;
        }
        if p.relay_id.is_none() {
            self.drop_peer(peer);
            return;
        }
        if p.restarts >= MAX_RESTARTS {
            return;
        }
        p.restarts += 1;
        tracing::debug!(peer, attempt = p.restarts, "restarting ice");
        self.restart(peer).await;
        self.schedule_recover(peer, RESTART_EVERY);
    }

    /// Reads audio levels off the connections in the background; the result
    /// comes back as `Internal::Levels`.
    fn read_levels(&mut self) {
        if self.levels_pending {
            return;
        }
        let pcs: Vec<(PeerId, PeerConnection)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.announced_connected && !p.lost)
            .filter_map(|(&id, p)| Some((id, p.pc.clone()?)))
            .collect();
        if pcs.is_empty() {
            if !self.speaking.is_empty() {
                self.on_levels(None, Vec::new());
            }
            return;
        }
        self.levels_pending = true;
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let mut me = None;
            let mut peers = Vec::with_capacity(pcs.len());
            for (id, pc) in pcs {
                let pulse = stats::pulse(&pc).await;
                me = me.or(pulse.sent);
                peers.push((id, pulse));
            }
            let _ = internal.send(Internal::Levels { me, peers });
        });
    }

    fn on_levels(&mut self, me: Option<f64>, peers: Vec<(PeerId, stats::Pulse)>) {
        self.levels_pending = false;
        let now = Instant::now();
        self.judge_quality(now, &peers);
        let mic_on = self.media_state.mic;
        let heard = me
            .filter(|_| mic_on)
            .map(|level| (self.me, level))
            .into_iter()
            .chain(peers.into_iter().filter_map(|(id, pulse)| {
                let level = pulse.received?;
                let p = self
                    .peers
                    .get(&id)
                    .filter(|p| p.verified && !p.muted_for_me)?;
                // The level is measured after the volume set for them; undo
                // it, so a friend turned down still shows as talking (at 0
                // they're silent, like Mute for me).
                let volume = p.volume.unwrap_or(1.0);
                Some((id, if volume > 0.0 { level / volume } else { 0.0 }))
            }));
        for (id, level) in heard {
            if level >= SPEAKING_LEVEL {
                self.loud_at.insert(id, now);
            }
        }
        let me = self.me;
        let peers = &self.peers;
        self.loud_at.retain(|id, at| {
            now.duration_since(*at) < SPEAKING_HOLD && (*id == me || peers.contains_key(id))
        });
        if !mic_on {
            self.loud_at.remove(&me);
        }
        let mut speaking: Vec<PeerId> = self.loud_at.keys().copied().collect();
        speaking.sort_unstable();
        if speaking != self.speaking {
            self.speaking = speaking.clone();
            self.emit(RoomEvent::Speaking { peers: speaking });
        }
    }

    /// Updates each friend's connection quality from the latest readings.
    /// Friends without a reading (reconnecting) start their meter over.
    fn judge_quality(&mut self, now: Instant, readings: &[(PeerId, stats::Pulse)]) {
        let mut changes = Vec::new();
        for (&id, p) in &mut self.peers {
            match readings.iter().find(|(peer, _)| *peer == id) {
                Some((_, pulse)) => {
                    if let Some(quality) = p.quality.feed(now, pulse.rtt_ms, pulse.audio_packets) {
                        changes.push((id, quality));
                    }
                }
                None => p.quality.reset(),
            }
        }
        for (peer, quality) in changes {
            if self.peers.get(&peer).is_some_and(|p| p.verified) {
                self.emit(RoomEvent::ConnectionQuality { peer, quality });
            }
        }
    }

    fn send_channel(&self, peer: PeerId, msg: &ChannelMsg) {
        let Some(dc) = self.peers.get(&peer).and_then(|p| p.dc.as_ref()) else {
            return;
        };
        if dc.state() != DataChannelState::Open {
            return;
        }
        let json = serde_json::to_vec(msg).expect("serializable");
        if dc.send(&json, false).is_err() {
            tracing::debug!(peer, "data channel send failed");
        }
    }

    fn broadcast(&self, msg: &ChannelMsg) {
        for (&peer, p) in &self.peers {
            if p.verified {
                self.send_channel(peer, msg);
            }
        }
    }

    fn broadcast_media(&self) {
        self.broadcast(&ChannelMsg::Media(self.media_state));
    }

    fn on_channel(&self, peer: PeerId, data: &[u8]) {
        if !self.peers.get(&peer).is_some_and(|p| p.verified) {
            return;
        }
        match serde_json::from_slice::<ChannelMsg>(data) {
            Ok(ChannelMsg::Chat { text, sent_at_ms }) => {
                let text: String = text.chars().take(MAX_CHAT_CHARS).collect();
                self.emit(RoomEvent::Chat {
                    from: peer,
                    text,
                    sent_at_ms,
                });
            }
            Ok(ChannelMsg::Media(state)) => self.emit(RoomEvent::PeerMedia { peer, state }),
            Err(_) => tracing::debug!(peer, "ignoring undecodable channel message"),
        }
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::SendFile(name, data) => self.send_file(name, data),
            Command::MuteFriend(peer, muted) => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.muted_for_me = muted;
                    p.apply_audio();
                }
            }
            Command::FriendVolume(peer, volume) => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.volume = Some(volume);
                    p.apply_audio();
                }
            }
            Command::Restart => {
                let peers: Vec<PeerId> = self
                    .peers
                    .iter()
                    .filter(|(_, p)| p.verified && p.relay_id.is_some())
                    .map(|(&id, _)| id)
                    .collect();
                for peer in peers {
                    self.restart(peer).await;
                }
            }
            Command::Chat(text) => {
                let text: String = text.chars().take(MAX_CHAT_CHARS).collect();
                if !text.trim().is_empty() {
                    self.broadcast(&ChannelMsg::Chat {
                        text,
                        sent_at_ms: now_ms(),
                    });
                }
            }
            Command::Mic(on) => {
                self.media.audio.set_enabled(on);
                self.media_state.mic = on;
                self.broadcast_media();
            }
            Command::Camera(target) => {
                let on_fatal = self.fatal_reporter(VideoKind::Camera);
                let video = self.media.video(VideoKind::Camera);
                video.producer = None;
                let (source, encoding) = (video.source.clone(), video.encoding);
                video.producer = match target.clone() {
                    None | Some(CameraTarget::External) => None,
                    Some(CameraTarget::TestPattern) => {
                        Some(Producer::spawn("silent-camera", move |stop| {
                            test_pattern(source, encoding, stop)
                        }))
                    }
                    #[cfg(desktop)]
                    Some(CameraTarget::Device(index)) => {
                        Some(Producer::spawn("silent-camera", move |stop| {
                            crate::camera::run(source, encoding, index, stop, on_fatal)
                        }))
                    }
                    #[cfg(not(desktop))]
                    Some(CameraTarget::Device(_)) => {
                        on_fatal("use CameraTarget::External on this platform".into());
                        None
                    }
                };
                self.media_state.camera = target.is_some();
                self.sync_senders(None);
                self.broadcast_media();
            }
            Command::Screen(target) => {
                let on_fatal = self.fatal_reporter(VideoKind::Screen);
                let video = self.media.video(VideoKind::Screen);
                video.producer = None;
                let (source, encoding) = (video.source.clone(), video.encoding);
                video.producer = match target.clone() {
                    None | Some(ScreenTarget::External) => None,
                    Some(ScreenTarget::TestPattern) => {
                        Some(Producer::spawn("silent-screen", move |stop| {
                            test_pattern(source, encoding, stop)
                        }))
                    }
                    #[cfg(desktop)]
                    Some(target) => Some(Producer::spawn("silent-screen", move |stop| {
                        crate::capture::run(source, encoding, target, stop, on_fatal)
                    })),
                    #[cfg(not(desktop))]
                    Some(_) => {
                        on_fatal("use ScreenTarget::External on this platform".into());
                        None
                    }
                };
                self.media_state.screen = target.is_some();
                self.sync_senders(None);
                self.broadcast_media();
            }
            Command::Leave(_) | Command::Stats(_) => unreachable!("handled in run loop"),
        }
    }

    fn fatal_reporter(&self, kind: VideoKind) -> impl Fn(String) + Send + 'static {
        let internal = self.internal.clone();
        move |message| {
            let _ = internal.send(Internal::CaptureFailed { kind, message });
        }
    }

    fn shutdown(&mut self) {
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in &peers {
            if self.peers.get(peer).is_some_and(|p| p.verified) {
                self.send_signal(*peer, &Signal::Bye);
            }
        }
        self.stop_everything();
    }

    /// Stops capture before closing connections, so closing doesn't race a
    /// stream of new frames through encoders that are being torn down.
    fn stop_everything(&mut self) {
        self.media.camera.producer = None;
        self.media.screen.producer = None;
        self.peers.clear();
    }
}
