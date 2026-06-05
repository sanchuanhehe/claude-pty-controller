//! Integration: Noise XXpsk3 handshake + AEAD app messages routed end-to-end
//! through the real `relay` binary. Asserts decryption works both ways and that
//! the relay only ever carries opaque ciphertext (zero-knowledge). (§13/§14.)

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use claude_pty_controller::e2ee::{
    derive_psk, AuthorizedDevices, Handshake, StaticKey,
};
use claude_pty_controller::relay_proto::{Envelope, Role, CONTROLLER_PEER};
use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct RelayProc(Child);
impl Drop for RelayProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn send(ws: &mut Ws, env: &Envelope) {
    ws.send(Message::Text(env.to_json())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> Envelope {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(e) = serde_json::from_str::<Envelope>(&t) {
                    return e;
                }
            }
            Some(Ok(_)) => continue,
            other => panic!("ws closed/err waiting for envelope: {other:?}"),
        }
    }
}

/// Receive until a `Deliver`, returning its `data` (skips PeerJoin/Joined/etc).
async fn recv_data(ws: &mut Ws) -> String {
    loop {
        if let Envelope::Deliver { data, .. } = recv(ws).await {
            return data;
        }
    }
}

#[tokio::test]
async fn e2ee_handshake_and_messages_through_relay() {
    let port = 50_000 + (std::process::id() % 10_000) as u16;
    let addr = format!("127.0.0.1:{port}");
    let url = format!("ws://{addr}");
    let token = "relay-secret-token";

    let _relay = RelayProc(
        Command::new(env!("CARGO_BIN_EXE_relay"))
            .env("RELAY_ADDR", &addr)
            .env("RELAY_TOKEN", token)
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("spawn relay"),
    );

    // Wait for the relay to listen.
    let mut ctrl = None;
    for _ in 0..50 {
        if let Ok((ws, _)) = connect_async(&url).await {
            ctrl = Some(ws);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut ctrl: Ws = ctrl.expect("relay never came up");

    let room = "test-room".to_string();
    // Controller joins.
    send(&mut ctrl, &Envelope::Join {
        room: room.clone(),
        role: Role::Controller,
        peer: CONTROLLER_PEER.into(),
        token: Some(token.into()),
    })
    .await;
    assert!(matches!(recv(&mut ctrl).await, Envelope::Joined));

    // Dashboard joins.
    let (mut dash, _) = connect_async(&url).await.unwrap();
    send(&mut dash, &Envelope::Join {
        room: room.clone(),
        role: Role::Dashboard,
        peer: "dash-1".into(),
        token: Some(token.into()),
    })
    .await;
    assert!(matches!(recv(&mut dash).await, Envelope::Joined));
    // Controller is told a peer joined.
    assert!(matches!(recv(&mut ctrl).await, Envelope::PeerJoin { peer } if peer == "dash-1"));

    // ---- Noise XXpsk3 handshake, routed through the relay ----
    let psk = derive_psk("0123456789abcdef0123456789abcdef0123");
    let ctrl_key = StaticKey::generate().unwrap();
    let dash_key = StaticKey::generate().unwrap();
    let mut hs_d = Handshake::initiator(&dash_key.private().unwrap(), &psk).unwrap(); // dashboard initiates
    let mut hs_c = Handshake::responder(&ctrl_key.private().unwrap(), &psk).unwrap();

    // m1: dash -> ctrl
    let m1 = hs_d.write().unwrap();
    send(&mut dash, &Envelope::Msg { to: CONTROLLER_PEER.into(), data: B64.encode(&m1) }).await;
    let got = recv_data(&mut ctrl).await;
    assert_ne!(got, B64.encode(b"plaintext"), "relay carries opaque bytes");
    hs_c.read(&B64.decode(got).unwrap()).unwrap();

    // m2: ctrl -> dash
    let m2 = hs_c.write().unwrap();
    send(&mut ctrl, &Envelope::Msg { to: "dash-1".into(), data: B64.encode(&m2) }).await;
    hs_d.read(&B64.decode(recv_data(&mut dash).await).unwrap()).unwrap();

    // m3: dash -> ctrl (carries dashboard static key + psk proof)
    let m3 = hs_d.write().unwrap();
    send(&mut dash, &Envelope::Msg { to: CONTROLLER_PEER.into(), data: B64.encode(&m3) }).await;
    hs_c.read(&B64.decode(recv_data(&mut ctrl).await).unwrap()).unwrap();

    assert!(hs_c.is_finished() && hs_d.is_finished());

    // Controller authorizes the dashboard's static key (TOFU-under-PSK enrollment).
    let dir = std::env::temp_dir().join(format!("cpc-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut authz = AuthorizedDevices::load(&dir.join("authorized.json")).unwrap();
    let dash_remote = hs_c.remote_static().unwrap();
    assert_eq!(dash_remote, dash_key.public().unwrap());
    assert!(!authz.contains(&dash_remote));
    authz.add(&dash_remote, "dash-1", 1).unwrap();
    assert!(authz.contains(&dash_remote)); // would be the revocation gate on reconnect

    let mut t_c = hs_c.into_transport().unwrap();
    let mut t_d = hs_d.into_transport().unwrap();

    // ---- App messages, encrypted end-to-end ----
    let ct = t_d.encrypt(b"input: refactor this").unwrap();
    send(&mut dash, &Envelope::Msg { to: CONTROLLER_PEER.into(), data: B64.encode(&ct) }).await;
    let wire = recv_data(&mut ctrl).await; // what the relay forwarded
    let wire_bytes = B64.decode(&wire).unwrap();
    assert!(!wire_bytes.windows(5).any(|w| w == b"input"), "ciphertext must not contain plaintext");
    assert_eq!(t_c.decrypt(&wire_bytes).unwrap(), b"input: refactor this");

    let ct2 = t_c.encrypt(b"output: done").unwrap();
    send(&mut ctrl, &Envelope::Msg { to: "dash-1".into(), data: B64.encode(&ct2) }).await;
    assert_eq!(t_d.decrypt(&B64.decode(recv_data(&mut dash).await).unwrap()).unwrap(), b"output: done");

    let _ = std::fs::remove_dir_all(&dir);
}
