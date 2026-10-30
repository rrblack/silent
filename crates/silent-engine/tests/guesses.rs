//! A relay that makes up joiners must not get unlimited guesses at the code.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use silent_core::proto::{ClientMsg, IceConfig, PeerFrame, ServerMsg, JOIN_BURST};
use silent_core::{InviteCode, PairHandshake};
use silent_engine::{AudioInput, CameraProfile, Engine, ScreenProfile, Settings};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

async fn send(ws: &mut WebSocketStream<TcpStream>, msg: ServerMsg) {
    let text = serde_json::to_string(&msg).unwrap();
    ws.send(Message::Text(text.into())).await.unwrap();
}

/// Waits briefly for the room's sealed `Hello` to `peer`: the message that
/// would let a fake joiner check its guess of the code.
async fn hello_to(ws: &mut WebSocketStream<TcpStream>, peer: u32) -> bool {
    let wait = tokio::time::sleep(Duration::from_millis(300));
    tokio::pin!(wait);
    loop {
        tokio::select! {
            _ = &mut wait => return false,
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMsg::Relay { to, data }) = serde_json::from_str(&text) {
                        if to == peer && matches!(PeerFrame::decode(&data), Ok(PeerFrame::Sealed(_))) {
                            return true;
                        }
                    }
                }
                Some(Ok(_)) => {}
                _ => panic!("the room hung up"),
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn made_up_joiners_get_a_limited_number_of_guesses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let relay = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        send(
            &mut ws,
            ServerMsg::Welcome {
                you: 1,
                peers: vec![],
                ice: IceConfig {
                    ice_servers: vec![],
                },
            },
        )
        .await;
        let guess: InviteCode = "1-oak-oak-oak".parse().unwrap();
        let mut hellos = 0;
        for peer in 2..2 + 2 * JOIN_BURST {
            send(&mut ws, ServerMsg::Joined { peer }).await;
            let (_, pake) = PairHandshake::start(&guess, peer, 1);
            let data = PeerFrame::Pake(pake).encode();
            send(&mut ws, ServerMsg::Relay { from: peer, data }).await;
            if hello_to(&mut ws, peer).await {
                hellos += 1;
            }
            send(&mut ws, ServerMsg::Left { peer }).await;
        }
        hellos
    });

    let engine = Engine::new();
    let settings = Settings {
        relay_url: url,
        name: "Alice".into(),
        audio: AudioInput::Silent,
        camera: CameraProfile::Standard,
        screen: ScreenProfile::Motion,
        relay_only: false,
    };
    let _room = engine.create_room(settings).await.unwrap();
    let hellos = relay.await.unwrap();
    // The budget refills by one every few seconds while the test runs.
    assert!(
        (JOIN_BURST..=JOIN_BURST + 3).contains(&hellos),
        "{hellos} guesses"
    );
}
