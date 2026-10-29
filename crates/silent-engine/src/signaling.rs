//! WebSocket client for the relay.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use silent_core::proto::{room_path, ClientMsg, IceConfig, Intent, RelayErrorCode, ServerMsg};
use silent_core::PeerId;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const KEEPALIVE: Duration = Duration::from_secs(15);
/// A socket that has heard nothing (not even a pong) for this long is treated
/// as dead, so a network change is noticed without waiting for TCP to give up.
const SILENCE: Duration = Duration::from_secs(45);
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
                Ok(ServerMsg::Welcome { you, peers, ice }) => {
                    break Welcome {
                        me: you,
                        peers,
                        ice,
                    }
                }
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
        let mut heard = tokio::time::Instant::now();
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
                        heard = tokio::time::Instant::now();
                        if let Ok(msg) = serde_json::from_str::<ServerMsg>(&t) {
                            if in_tx.send(msg).is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => heard = tokio::time::Instant::now(),
                },
                _ = keepalive.tick() => {
                    if heard.elapsed() > SILENCE {
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    Ok((
        welcome,
        SignalLink {
            outbound: out_tx,
            inbound: in_rx,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `wss://` relay needs a TLS crypto provider. Without one rustls
    /// panics inside the connect task and the app waits forever.
    #[tokio::test]
    async fn tls_relay_fails_cleanly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept and hang up, so the TLS handshake starts and fails.
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let url = format!("https://127.0.0.1:{port}");
        let result = tokio::spawn(async move { connect(&url, 1234, Intent::Create).await })
            .await
            .expect("connect must not panic");
        assert!(matches!(result, Err(SignalError::Connect(_))));
    }
}
