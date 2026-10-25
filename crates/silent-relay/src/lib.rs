//! Signaling relay.
//!
//! Same protocol as the Cloudflare Worker in `signal/`. Rooms live in memory
//! only and disappear when the last peer leaves. Nothing is logged except
//! join/leave events at debug level. The relay never sees names, SDP or ICE
//! candidates: those are sealed with keys only the peers hold.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use silent_core::code::NAMEPLATE_MAX;
use silent_core::proto::{
    ClientMsg, IceConfig, Intent, RelayErrorCode, ServerMsg, JOIN_BURST, JOIN_REFILL_SECS,
    MAX_PEERS, MAX_RELAY_DATA,
};
use silent_core::PeerId;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

const PEER_QUEUE: usize = 256;
const RATE_BURST: f64 = 200.0;
const RATE_PER_SEC: f64 = 50.0;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
pub struct RelayConfig {
    pub ice: IceConfig,
}

struct Room {
    next_id: PeerId,
    /// Ids only grow, so a lower id joined earlier.
    peers: BTreeMap<PeerId, mpsc::Sender<ServerMsg>>,
    /// Who asked for whom to be removed: target → voters.
    evict_votes: HashMap<PeerId, BTreeSet<PeerId>>,
    /// Join attempts left, refilled over time (see `JOIN_BURST`).
    joins: f64,
    joins_at: Instant,
}

impl Default for Room {
    fn default() -> Self {
        Self {
            next_id: 0,
            peers: BTreeMap::new(),
            evict_votes: HashMap::new(),
            joins: f64::from(JOIN_BURST),
            joins_at: Instant::now(),
        }
    }
}

impl Room {
    /// Removes `target` when everyone who joined before them asked to.
    fn settle_eviction(&mut self, target: PeerId) {
        let Some(votes) = self.evict_votes.get(&target) else {
            return;
        };
        let mut earlier = self.peers.range(..target).map(|(id, _)| id).peekable();
        if earlier.peek().is_none() || !earlier.all(|id| votes.contains(id)) {
            return;
        }
        if let Some(tx) = self.remove(target) {
            let _ = tx.try_send(ServerMsg::Error {
                code: RelayErrorCode::Evicted,
            });
        }
    }

    /// Takes `peer` out of the room and tells the others.
    fn remove(&mut self, peer: PeerId) -> Option<mpsc::Sender<ServerMsg>> {
        let tx = self.peers.remove(&peer)?;
        self.evict_votes.remove(&peer);
        for voters in self.evict_votes.values_mut() {
            voters.remove(&peer);
        }
        for other in self.peers.values() {
            let _ = other.try_send(ServerMsg::Left { peer });
        }
        // Someone who had not voted may have been all that stood in the way.
        let targets: Vec<PeerId> = self.evict_votes.keys().copied().collect();
        for target in targets {
            self.settle_eviction(target);
        }
        Some(tx)
    }

    fn take_join(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.joins_at).as_secs_f64() / f64::from(JOIN_REFILL_SECS);
        self.joins = (self.joins + refill).min(f64::from(JOIN_BURST));
        self.joins_at = now;
        if self.joins < 1.0 {
            return false;
        }
        self.joins -= 1.0;
        true
    }
}

#[derive(Clone)]
struct AppState {
    rooms: Arc<Mutex<HashMap<u16, Room>>>,
    config: Arc<RelayConfig>,
}

struct Admitted {
    me: PeerId,
    peers: Vec<PeerId>,
    tx: mpsc::Sender<ServerMsg>,
    rx: mpsc::Receiver<ServerMsg>,
}

pub fn router(config: RelayConfig) -> Router {
    let state = AppState {
        rooms: Arc::default(),
        config: Arc::new(config),
    };
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/rooms/{nameplate}", get(room))
        .with_state(state)
}

pub async fn serve(listener: TcpListener, config: RelayConfig) -> std::io::Result<()> {
    axum::serve(listener, router(config)).await
}

async fn room(
    ws: WebSocketUpgrade,
    Path(nameplate): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Response {
    let nameplate = nameplate
        .parse::<u16>()
        .ok()
        .filter(|n| (1..=NAMEPLATE_MAX).contains(n));
    let intent = query.get("intent").and_then(|i| Intent::parse(i));
    ws.max_message_size(MAX_RELAY_DATA + 1024)
        .on_upgrade(move |socket| async move {
            match (nameplate, intent) {
                (Some(n), Some(i)) => run_peer(socket, state, n, i).await,
                _ => reject(socket, RelayErrorCode::BadRequest).await,
            }
        })
        .into_response()
}

async fn reject(mut socket: WebSocket, code: RelayErrorCode) {
    let _ = socket.send(text(&ServerMsg::Error { code })).await;
    let _ = socket.close().await;
}

fn text(msg: &ServerMsg) -> Message {
    Message::Text(Utf8Bytes::from(
        serde_json::to_string(msg).expect("serializable"),
    ))
}

async fn admit(
    state: &AppState,
    nameplate: u16,
    intent: Intent,
) -> Result<Admitted, RelayErrorCode> {
    let mut rooms = state.rooms.lock().await;
    let exists = rooms.get(&nameplate).is_some_and(|r| !r.peers.is_empty());
    match intent {
        Intent::Create if exists => return Err(RelayErrorCode::RoomTaken),
        Intent::Join if !exists => return Err(RelayErrorCode::NoRoom),
        _ => {}
    }
    let room = rooms.entry(nameplate).or_default();
    if intent == Intent::Join && !room.take_join() {
        return Err(RelayErrorCode::TooManyJoins);
    }
    if room.peers.len() >= MAX_PEERS {
        return Err(RelayErrorCode::RoomFull);
    }
    room.next_id += 1;
    let me = room.next_id;
    let peers: Vec<PeerId> = room.peers.keys().copied().collect();
    for tx in room.peers.values() {
        let _ = tx.try_send(ServerMsg::Joined { peer: me });
    }
    let (tx, rx) = mpsc::channel(PEER_QUEUE);
    room.peers.insert(me, tx.clone());
    Ok(Admitted { me, peers, tx, rx })
}

async fn depart(state: &AppState, nameplate: u16, me: PeerId) {
    let mut rooms = state.rooms.lock().await;
    if let Some(room) = rooms.get_mut(&nameplate) {
        room.remove(me);
        if room.peers.is_empty() {
            rooms.remove(&nameplate);
        }
    }
}

async fn vote_evict(state: &AppState, nameplate: u16, voter: PeerId, target: PeerId) {
    let mut rooms = state.rooms.lock().await;
    let Some(room) = rooms.get_mut(&nameplate) else {
        return;
    };
    // Only votes from people who were there first count.
    if voter < target && room.peers.contains_key(&target) {
        room.evict_votes.entry(target).or_default().insert(voter);
        room.settle_eviction(target);
    }
}

async fn forward(state: &AppState, nameplate: u16, from: PeerId, to: PeerId, data: String) {
    let rooms = state.rooms.lock().await;
    if let Some(tx) = rooms.get(&nameplate).and_then(|r| r.peers.get(&to)) {
        let _ = tx.try_send(ServerMsg::Relay { from, data });
    }
}

async fn run_peer(socket: WebSocket, state: AppState, nameplate: u16, intent: Intent) {
    let Admitted {
        me,
        peers,
        tx,
        mut rx,
    } = match admit(&state, nameplate, intent).await {
        Ok(admitted) => admitted,
        Err(code) => return reject(socket, code).await,
    };
    tracing::debug!(room = nameplate, peer = me, "joined");

    let (mut sink, mut stream) = socket.split();
    let _ = tx.try_send(ServerMsg::Welcome {
        you: me,
        peers,
        ice: state.config.ice.clone(),
    });
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let last = matches!(msg, ServerMsg::Error { .. });
            if sink.send(text(&msg)).await.is_err() || last {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut tokens = RATE_BURST;
    let mut last = Instant::now();
    let mut writer_done = false;
    let error = loop {
        let msg = tokio::select! {
            next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => match next {
                Ok(Some(Ok(msg))) => msg,
                _ => break None,
            },
            _ = &mut writer => {
                writer_done = true;
                break None;
            }
        };
        let now = Instant::now();
        tokens = (tokens + now.duration_since(last).as_secs_f64() * RATE_PER_SEC).min(RATE_BURST);
        last = now;
        if tokens < 1.0 {
            break Some(RelayErrorCode::RateLimited);
        }
        tokens -= 1.0;

        match msg {
            Message::Text(body) => match serde_json::from_str::<ClientMsg>(&body) {
                Ok(ClientMsg::Relay { data, .. }) if data.len() > MAX_RELAY_DATA => {
                    break Some(RelayErrorCode::TooLarge)
                }
                Ok(ClientMsg::Relay { to, data }) if to != me => {
                    forward(&state, nameplate, me, to, data).await
                }
                Ok(ClientMsg::Evict { peer }) if peer != me => {
                    vote_evict(&state, nameplate, me, peer).await
                }
                _ => break Some(RelayErrorCode::BadRequest),
            },
            Message::Close(_) => break None,
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) => break Some(RelayErrorCode::BadRequest),
        }
    };

    depart(&state, nameplate, me).await;
    if let Some(code) = error {
        let _ = tx.try_send(ServerMsg::Error { code });
    }
    drop(tx);
    if !writer_done
        && tokio::time::timeout(DRAIN_TIMEOUT, &mut writer)
            .await
            .is_err()
    {
        writer.abort();
    }
    tracing::debug!(room = nameplate, peer = me, "left");
}
