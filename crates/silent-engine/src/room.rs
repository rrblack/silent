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

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use libwebrtc::peer_connection::PeerConnection;
use libwebrtc::peer_connection_factory::PeerConnectionFactory;
use libwebrtc::prelude::*;
use libwebrtc::video_track::RtcVideoTrack;
use silent_core::proto::{
    clean_name, ChannelMsg, ClientMsg, IceConfig, MediaState, PeerFrame, ServerMsg, Signal,
    MAX_CHAT_CHARS, PROTOCOL_VERSION,
};
use silent_core::{
    fingerprint_from_sdp, InviteCode, Opener, PairHandshake, PairKeys, PeerId, Sealer,
};
use tokio::sync::{mpsc, oneshot};

use crate::media::{
    apply_encoding, test_pattern, AudioInput, CameraProfile, CameraTarget, LocalMedia, Producer,
    ScreenProfile, ScreenTarget, VideoInput, VideoKind, STREAM_AUDIO,
};
use crate::signaling::{SignalLink, Welcome};
use crate::stats::{self, PeerStats};

const START_BITRATE_BPS: u64 = 1_200_000;

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
    },
    /// They typed a different code. Nothing further is exchanged with them.
    PeerWrongCode {
        peer: PeerId,
    },
    /// Direct encrypted connection is up.
    PeerConnected {
        peer: PeerId,
        safety_words: [&'static str; 4],
    },
    PeerConnectionLost {
        peer: PeerId,
    },
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
    /// The relay connection dropped. Existing calls keep working; nobody new
    /// can join until the room is recreated.
    RelayLost,
}

enum Command {
    Chat(String),
    Mic(bool),
    Camera(Option<CameraTarget>),
    Screen(Option<ScreenTarget>),
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
        track: MediaStreamTrack,
    },
    DcOpen {
        peer: PeerId,
    },
    DcMessage {
        peer: PeerId,
        data: Vec<u8>,
    },
    CaptureFailed {
        kind: VideoKind,
        message: String,
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
            platform_audio: media.has_platform_audio(),
        };

        let actor = Actor {
            code,
            me: welcome.me,
            name: clean_name(&settings.name),
            factory,
            rtc_config: rtc_config(&welcome.ice, settings.relay_only),
            media,
            media_state: MediaState {
                mic: true,
                camera: false,
                screen: false,
            },
            peers: HashMap::new(),
            relay: link.outbound,
            events: ev_tx,
            internal: int_tx,
        };
        tokio::spawn(actor.run(welcome.peers, link.inbound, cmd_rx, int_rx));
        room
    }

    pub fn code(&self) -> &InviteCode {
        &self.code
    }

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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct Peer {
    handshake: Option<PairHandshake>,
    keys: Option<PairKeys>,
    sealer: Option<Sealer>,
    opener: Option<Opener>,
    verified: bool,
    rejected: bool,
    pc: Option<PeerConnection>,
    dc: Option<DataChannel>,
    remote_described: bool,
    pending_ice: Vec<(String, i32, String)>,
    remote_fingerprint: Option<String>,
    announced_connected: bool,
    video_senders: Vec<(VideoKind, RtpSender)>,
}

struct Actor {
    code: InviteCode,
    me: PeerId,
    name: String,
    factory: PeerConnectionFactory,
    rtc_config: RtcConfiguration,
    media: LocalMedia,
    media_state: MediaState,
    peers: HashMap<PeerId, Peer>,
    relay: mpsc::UnboundedSender<ClientMsg>,
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
        loop {
            tokio::select! {
                msg = relay_in.recv(), if relay_open => match msg {
                    Some(msg) => self.on_relay(msg).await,
                    None => {
                        relay_open = false;
                        self.emit(RoomEvent::RelayLost);
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
                    Some(cmd) => self.on_command(cmd),
                    None => {
                        self.shutdown();
                        return;
                    }
                },
                Some(ev) = internal.recv() => self.on_internal(ev).await,
            }
        }
    }

    fn emit(&self, event: RoomEvent) {
        let _ = self.events.send(event);
    }

    fn relay_frame(&self, to: PeerId, frame: PeerFrame) {
        let _ = self.relay.send(ClientMsg::Relay {
            to,
            data: frame.encode(),
        });
    }

    fn send_signal(&mut self, to: PeerId, signal: &Signal) {
        let Some(sealer) = self.peers.get_mut(&to).and_then(|p| p.sealer.as_mut()) else {
            return;
        };
        let plaintext = serde_json::to_vec(signal).expect("serializable");
        let sealed = sealer.seal(&plaintext);
        self.relay_frame(to, PeerFrame::Sealed(sealed));
    }

    fn begin_pair(&mut self, peer: PeerId) {
        if peer == self.me || self.peers.contains_key(&peer) {
            return;
        }
        let (handshake, outbound) = PairHandshake::start(&self.code, self.me, peer);
        self.peers.insert(
            peer,
            Peer {
                handshake: Some(handshake),
                ..Default::default()
            },
        );
        self.relay_frame(peer, PeerFrame::Pake(outbound));
        self.emit(RoomEvent::PeerJoining { peer });
    }

    async fn on_relay(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Joined { peer } => self.begin_pair(peer),
            ServerMsg::Left { peer } => self.drop_peer(peer),
            ServerMsg::Relay { from, data } => match PeerFrame::decode(&data) {
                Ok(PeerFrame::Pake(msg)) => self.on_pake(from, &msg),
                Ok(PeerFrame::Sealed(sealed)) => self.on_sealed(from, &sealed).await,
                Err(_) => tracing::debug!(peer = from, "ignoring malformed frame"),
            },
            ServerMsg::Welcome { .. } | ServerMsg::Error { .. } => {}
        }
    }

    fn on_pake(&mut self, from: PeerId, msg: &[u8]) {
        self.begin_pair(from);
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
                    self.reject(from);
                }
                return;
            }
        };
        let Ok(signal) = serde_json::from_slice::<Signal>(&plaintext) else {
            tracing::debug!(peer = from, "ignoring undecodable signal");
            return;
        };
        if let Err(e) = self.on_signal(from, signal).await {
            tracing::warn!(peer = from, "negotiation failed: {e}");
        }
    }

    fn reject(&mut self, peer: PeerId) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.rejected = true;
            if let Some(pc) = p.pc.take() {
                pc.close();
            }
            p.dc = None;
        }
        self.emit(RoomEvent::PeerWrongCode { peer });
    }

    async fn on_signal(&mut self, from: PeerId, signal: Signal) -> anyhow::Result<()> {
        match signal {
            Signal::Hello { version, name } => {
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
                self.emit(RoomEvent::PeerVerified {
                    peer: from,
                    name: clean_name(&name),
                });
                if self.me > from {
                    self.send_offer(from).await?;
                }
            }
            Signal::Offer { sdp } => {
                let pc = self.pc(from)?;
                let offer = SessionDescription::parse(&sdp, SdpType::Offer)?;
                pc.set_remote_description(offer).await?;
                self.add_local_tracks(from, &pc)?;
                let answer = pc.create_answer(AnswerOptions::default()).await?;
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

    async fn send_offer(&mut self, peer: PeerId) -> anyhow::Result<()> {
        let pc = self.pc(peer)?;
        let options = OfferOptions {
            ice_restart: false,
            offer_to_receive_audio: true,
            offer_to_receive_video: true,
        };
        let offer = pc.create_offer(options).await?;
        pc.set_local_description(offer.clone()).await?;
        self.send_signal(
            peer,
            &Signal::Offer {
                sdp: offer.to_string(),
            },
        );
        Ok(())
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
                        tracing::debug!(peer, "ice candidate rejected: {e}");
                    }
                }
                Err(e) => tracing::debug!(peer, "bad ice candidate: {e}"),
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
            let kind = ev
                .streams
                .iter()
                .map(|s| s.id())
                .chain(ev.receiver.stream_ids())
                .find_map(|id| VideoKind::from_stream_id(&id));
            tracing::debug!(peer, ?kind, mid = ?ev.transceiver.mid(), "remote track");
            let _ = tx.send(Internal::Track {
                peer,
                kind,
                track: ev.track,
            });
        })));

        let dc = pc.create_data_channel(
            "silent",
            DataChannelInit {
                negotiated: true,
                id: 0,
                ordered: true,
                ..Default::default()
            },
        )?;
        let tx = self.internal.clone();
        dc.on_state_change(Some(Box::new(move |state| {
            if state == DataChannelState::Open {
                let _ = tx.send(Internal::DcOpen { peer });
            }
        })));
        let tx = self.internal.clone();
        dc.on_message(Some(Box::new(move |buf: DataBuffer| {
            let _ = tx.send(Internal::DcMessage {
                peer,
                data: buf.data.to_vec(),
            });
        })));

        if self.me > peer {
            self.add_local_transceivers(peer, &pc)?;
        }

        let p = self.peers.get_mut(&peer).expect("peer exists");
        p.pc = Some(pc);
        p.dc = Some(dc);
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
        self.attach_senders(peer, senders);
        Ok(())
    }

    fn attach_senders(&mut self, peer: PeerId, senders: Vec<(VideoKind, RtpSender)>) {
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
        for (&id, p) in &self.peers {
            if only.is_some_and(|o| o != id) {
                continue;
            }
            for (kind, sender) in &p.video_senders {
                let (on, encoding) = match kind {
                    VideoKind::Camera => (self.media_state.camera, &self.media.camera.encoding),
                    VideoKind::Screen => (self.media_state.screen, &self.media.screen.encoding),
                };
                apply_encoding(sender, encoding, on);
            }
        }
    }

    fn drop_peer(&mut self, peer: PeerId) {
        let Some(p) = self.peers.remove(&peer) else {
            return;
        };
        if let Some(dc) = &p.dc {
            dc.close();
        }
        if let Some(pc) = &p.pc {
            pc.close();
        }
        if p.verified {
            self.emit(RoomEvent::PeerLeft { peer });
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
            Internal::Track { peer, kind, track } => {
                if let (Some(kind), MediaStreamTrack::Video(track)) = (kind, track) {
                    if self.peers.get(&peer).is_some_and(|p| p.verified) {
                        self.emit(RoomEvent::RemoteVideo { peer, kind, track });
                    }
                }
            }
            Internal::DcOpen { peer } => {
                let state = self.media_state;
                self.send_channel(peer, &ChannelMsg::Media(state));
            }
            Internal::DcMessage { peer, data } => self.on_channel(peer, &data),
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
        }
    }

    fn on_pc_state(&mut self, peer: PeerId, state: PeerConnectionState) {
        tracing::debug!(me = self.me, peer, ?state, "connection state");
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        match state {
            PeerConnectionState::Connected if !p.announced_connected => {
                p.announced_connected = true;
                let local =
                    p.pc.as_ref()
                        .and_then(|pc| pc.current_local_description())
                        .and_then(|d| fingerprint_from_sdp(&d.to_string()));
                let words = match (&p.keys, local, &p.remote_fingerprint) {
                    (Some(keys), Some(local), Some(remote)) => keys.safety_words(&local, remote),
                    _ => ["?", "?", "?", "?"],
                };
                self.emit(RoomEvent::PeerConnected {
                    peer,
                    safety_words: words,
                });
            }
            PeerConnectionState::Failed => {
                p.announced_connected = false;
                self.emit(RoomEvent::PeerConnectionLost { peer });
            }
            _ => {}
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

    fn on_command(&mut self, cmd: Command) {
        match cmd {
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
        for peer in peers {
            if let Some(p) = self.peers.remove(&peer) {
                if let Some(pc) = &p.pc {
                    pc.close();
                }
            }
        }
        self.media.camera.producer = None;
        self.media.screen.producer = None;
    }
}
