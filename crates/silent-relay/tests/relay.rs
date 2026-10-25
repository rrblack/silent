use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use silent_core::proto::{room_path, ClientMsg, Intent, RelayErrorCode, ServerMsg};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Runs against a local in-process relay, or against `SILENT_TEST_RELAY`
/// (e.g. `http://127.0.0.1:8787` for `wrangler dev`) to check that the
/// Worker speaks the exact same protocol.
async fn relay_base() -> String {
    if let Ok(base) = std::env::var("SILENT_TEST_RELAY") {
        return base;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(silent_relay::serve(listener, Default::default()));
    format!("http://{addr}")
}

fn unique_nameplate() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = (std::process::id() % 5000) as u16 + 1000;
    base + NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn connect(base: &str, nameplate: u16, intent: Intent) -> Ws {
    let url = format!(
        "{}{}",
        base.replacen("http", "ws", 1),
        room_path(nameplate, intent)
    );
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

async fn recv(ws: &mut Ws) -> ServerMsg {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for relay")
            .expect("socket closed")
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    ws.send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn room_lifecycle() {
    let base = relay_base().await;
    let n = unique_nameplate();

    let mut a = connect(&base, n, Intent::Create).await;
    let ServerMsg::Welcome {
        you: a_id,
        peers,
        ice,
    } = recv(&mut a).await
    else {
        panic!()
    };
    assert!(peers.is_empty());
    assert!(!ice.ice_servers.is_empty());

    let mut b = connect(&base, n, Intent::Join).await;
    let ServerMsg::Welcome {
        you: b_id, peers, ..
    } = recv(&mut b).await
    else {
        panic!()
    };
    assert_eq!(peers, vec![a_id]);
    assert_ne!(a_id, b_id);
    assert_eq!(recv(&mut a).await, ServerMsg::Joined { peer: b_id });

    send(
        &mut b,
        &ClientMsg::Relay {
            to: a_id,
            data: "hi".into(),
        },
    )
    .await;
    assert_eq!(
        recv(&mut a).await,
        ServerMsg::Relay {
            from: b_id,
            data: "hi".into()
        }
    );
    send(
        &mut a,
        &ClientMsg::Relay {
            to: b_id,
            data: "yo".into(),
        },
    )
    .await;
    assert_eq!(
        recv(&mut b).await,
        ServerMsg::Relay {
            from: a_id,
            data: "yo".into()
        }
    );

    b.close(None).await.unwrap();
    assert_eq!(recv(&mut a).await, ServerMsg::Left { peer: b_id });
    a.close(None).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut late = connect(&base, n, Intent::Join).await;
    assert_eq!(
        recv(&mut late).await,
        ServerMsg::Error {
            code: RelayErrorCode::NoRoom
        }
    );
}

#[tokio::test]
async fn create_on_busy_room_is_rejected() {
    let base = relay_base().await;
    let n = unique_nameplate();
    let mut a = connect(&base, n, Intent::Create).await;
    recv(&mut a).await;
    let mut b = connect(&base, n, Intent::Create).await;
    assert_eq!(
        recv(&mut b).await,
        ServerMsg::Error {
            code: RelayErrorCode::RoomTaken
        }
    );
}

#[tokio::test]
async fn fifth_peer_is_rejected() {
    let base = relay_base().await;
    let n = unique_nameplate();
    let mut peers = vec![connect(&base, n, Intent::Create).await];
    recv(&mut peers[0]).await;
    for _ in 0..3 {
        let mut p = connect(&base, n, Intent::Join).await;
        assert!(matches!(recv(&mut p).await, ServerMsg::Welcome { .. }));
        peers.push(p);
    }
    let mut fifth = connect(&base, n, Intent::Join).await;
    assert_eq!(
        recv(&mut fifth).await,
        ServerMsg::Error {
            code: RelayErrorCode::RoomFull
        }
    );
}

#[tokio::test]
async fn bad_requests_are_rejected() {
    let base = relay_base().await;
    let ws_base = base.replacen("http", "ws", 1);
    for path in [
        "/v1/rooms/0?intent=join",
        "/v1/rooms/abc?intent=join",
        "/v1/rooms/12?intent=nope",
    ] {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("{ws_base}{path}"))
            .await
            .unwrap();
        assert_eq!(
            recv(&mut ws).await,
            ServerMsg::Error {
                code: RelayErrorCode::BadRequest
            },
            "{path}"
        );
    }

    let n = unique_nameplate();
    let mut a = connect(&base, n, Intent::Create).await;
    recv(&mut a).await;
    a.send(Message::Text("not json".into())).await.unwrap();
    assert_eq!(
        recv(&mut a).await,
        ServerMsg::Error {
            code: RelayErrorCode::BadRequest
        }
    );
}

#[tokio::test]
async fn oversized_relay_data_is_rejected() {
    let base = relay_base().await;
    let n = unique_nameplate();
    let mut a = connect(&base, n, Intent::Create).await;
    let ServerMsg::Welcome { you, .. } = recv(&mut a).await else {
        panic!()
    };
    let mut b = connect(&base, n, Intent::Join).await;
    recv(&mut b).await;
    let data = "x".repeat(silent_core::proto::MAX_RELAY_DATA + 1);
    send(&mut b, &ClientMsg::Relay { to: you, data }).await;
    assert_eq!(
        recv(&mut b).await,
        ServerMsg::Error {
            code: RelayErrorCode::TooLarge
        }
    );
}

#[tokio::test]
async fn join_attempts_are_budgeted() {
    use silent_core::proto::JOIN_BURST;
    let base = relay_base().await;
    let n = unique_nameplate();
    let mut host = connect(&base, n, Intent::Create).await;
    recv(&mut host).await;
    for _ in 0..JOIN_BURST {
        let mut p = connect(&base, n, Intent::Join).await;
        assert!(matches!(recv(&mut p).await, ServerMsg::Welcome { .. }));
        p.close(None).await.unwrap();
        assert!(matches!(recv(&mut host).await, ServerMsg::Joined { .. }));
        assert!(matches!(recv(&mut host).await, ServerMsg::Left { .. }));
    }
    let mut extra = connect(&base, n, Intent::Join).await;
    assert_eq!(
        recv(&mut extra).await,
        ServerMsg::Error {
            code: RelayErrorCode::TooManyJoins
        }
    );
}
