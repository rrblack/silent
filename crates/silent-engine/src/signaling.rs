//! WebSocket client for the relay.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use silent_core::proto::{room_path, ClientMsg, IceConfig, Intent, RelayErrorCode, ServerMsg};
use silent_core::PeerId;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const KEEPALIVE: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("could not reach the relay: {0}")]
    Connect(String),
    #[error("{0}")]
    Relay(#[from] RelayErrorCode),
    #[error("relay closed the connection before welcoming us")]
    NoWelcome,
}

pub struct Welcome {
    pub me: PeerId,
    pub peers: Vec<PeerId>,
    pub ice: IceConfig,
}

pub struct SignalLink {
    pub outbound: mpsc::UnboundedSender<ClientMsg>,
    pub inbound: mpsc::UnboundedReceiver<ServerMsg>,
}

/// Connects to `relay_url` (http(s) or ws(s) base URL) and waits for the
/// welcome. Everything after the welcome is pumped through channels by a
/// background task that also keeps the socket alive.
pub async fn connect(
    relay_url: &str,
    nameplate: u16,
    intent: Intent,
) -> Result<(Welcome, SignalLink), SignalError> {
    let base = relay_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    let url = format!("{base}{}", room_path(nameplate, intent));

    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| SignalError::Connect("timed out".into()))?
        .map_err(|e| SignalError::Connect(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();

    let welcome = loop {
        let msg = tokio::time::timeout(CONNECT_TIMEOUT, stream.next())
            .await
            .map_err(|_| SignalError::NoWelcome)?;
        match msg {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(&t) {
                Ok(ServerMsg::Welcome { you, peers, ice }) => break Welcome { me: you, peers, ice },
                Ok(ServerMsg::Error { code }) => return Err(code.into()),
                _ => continue,
            },
            Some(Ok(_)) => continue,
            _ => return Err(SignalError::NoWelcome),
        }
    };

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerMsg>();

    tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.tick().await;
        loop {
            tokio::select! {
                out = out_rx.recv() => match out {
                    Some(msg) => {
                        let text = serde_json::to_string(&msg).expect("serializable");
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                incoming = stream.next() => match incoming {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(msg) = serde_json::from_str::<ServerMsg>(&t) {
                            if in_tx.send(msg).is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                },
                _ = keepalive.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    Ok((welcome, SignalLink { outbound: out_tx, inbound: in_rx }))
}
