//! Relay (ARCHITECTURE §13) — a stateless-per-room, zero-knowledge frame router.
//!
//! Endpoints (one controller + N dashboards) dial in, `Join` a room, and the
//! relay forwards opaque `Msg{to,data}` envelopes between them as `Deliver`. It
//! never inspects `data` (E2EE ciphertext). It notifies the controller of
//! dashboard join/leave so the controller can set up / tear down per-peer Noise
//! sessions. Per-connection bounded queues; a slow consumer is dropped, never
//! head-of-line-blocking the room (review RELAY-5).
//!
//! NOTE (review RELAY-2): room state is per-instance — run one instance per room
//! or use sticky routing; this is not horizontally stateless. `RELAY_TOKEN`
//! gates admission (review RELAY-4). The endpoint↔relay link should be wss in
//! production (terminate TLS at a proxy or extend this binary); payloads are
//! E2EE regardless.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use claude_pty_controller::relay_proto::{Envelope, Role, CONTROLLER_PEER};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type Tx = mpsc::Sender<Message>;

#[derive(Default)]
struct Room {
    controller: Option<Tx>,
    dashboards: HashMap<String, Tx>,
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = std::env::var("RELAY_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".into());
    let token = std::env::var("RELAY_TOKEN").ok().filter(|s| !s.is_empty());
    if token.is_none() {
        tracing::warn!("RELAY_TOKEN not set — admission is open (review RELAY-4 recommends setting it)");
    }
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "relay listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let rooms = rooms.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, rooms, token).await {
                tracing::debug!(%peer, error = %e, "connection ended");
            }
        });
    }
}

/// Deliver an envelope to a connection's queue (drop on full — slow consumer).
fn send(tx: &Tx, env: &Envelope) {
    let _ = tx.try_send(Message::Text(env.to_json()));
}

async fn handle(stream: tokio::net::TcpStream, rooms: Rooms, token: Option<String>) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(256);

    // Writer task.
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    // First frame must be Join.
    let first = stream.next().await;
    let join = match first {
        Some(Ok(Message::Text(t))) => serde_json::from_str::<Envelope>(&t).ok(),
        _ => None,
    };
    let (room_name, role, peer) = match join {
        Some(Envelope::Join { room, role, peer, token: client_token }) => {
            if token.is_some() && client_token != token {
                send(&tx, &Envelope::Error { msg: "bad RELAY_TOKEN".into() });
                return Ok(());
            }
            (room, role, peer)
        }
        _ => {
            send(&tx, &Envelope::Error { msg: "first frame must be join".into() });
            return Ok(());
        }
    };

    // Register.
    {
        let mut g = rooms.lock().unwrap();
        let room = g.entry(room_name.clone()).or_default();
        match role {
            Role::Controller => {
                if room.controller.is_some() {
                    drop(g);
                    send(&tx, &Envelope::Error { msg: "controller slot occupied".into() });
                    return Ok(());
                }
                room.controller = Some(tx.clone());
                send(&tx, &Envelope::Joined); // ack first
                // Tell the controller about existing dashboards, and tell each
                // dashboard the controller is now present (so it can initiate).
                for (p, dtx) in &room.dashboards {
                    send(&tx, &Envelope::PeerJoin { peer: p.clone() });
                    send(dtx, &Envelope::PeerJoin { peer: CONTROLLER_PEER.into() });
                }
            }
            Role::Dashboard => {
                room.dashboards.insert(peer.clone(), tx.clone());
                send(&tx, &Envelope::Joined); // ack first
                if let Some(c) = &room.controller {
                    send(c, &Envelope::PeerJoin { peer: peer.clone() });
                    send(&tx, &Envelope::PeerJoin { peer: CONTROLLER_PEER.into() });
                }
            }
        }
    }
    tracing::info!(room = %room_name, ?role, %peer, "joined");

    // Route loop.
    let my_id = match role {
        Role::Controller => CONTROLLER_PEER.to_string(),
        Role::Dashboard => peer.clone(),
    };
    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        if let Ok(Envelope::Msg { to, data }) = serde_json::from_str::<Envelope>(&text) {
            let g = rooms.lock().unwrap();
            if let Some(room) = g.get(&room_name) {
                let recipient = if to == CONTROLLER_PEER {
                    room.controller.clone()
                } else {
                    room.dashboards.get(&to).cloned()
                };
                drop(g);
                if let Some(rx_tx) = recipient {
                    send(&rx_tx, &Envelope::Deliver { from: my_id.clone(), data });
                }
            }
        }
    }

    // Cleanup.
    {
        let mut g = rooms.lock().unwrap();
        if let Some(room) = g.get_mut(&room_name) {
            match role {
                Role::Controller => {
                    room.controller = None;
                    for dtx in room.dashboards.values() {
                        send(dtx, &Envelope::PeerLeave { peer: CONTROLLER_PEER.into() });
                    }
                }
                Role::Dashboard => {
                    room.dashboards.remove(&peer);
                    if let Some(c) = &room.controller {
                        send(c, &Envelope::PeerLeave { peer: peer.clone() });
                    }
                }
            }
            if room.controller.is_none() && room.dashboards.is_empty() {
                g.remove(&room_name);
            }
        }
    }
    writer.abort();
    Ok(())
}
