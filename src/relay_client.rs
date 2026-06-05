//! Controller-side relay client with per-dashboard E2EE fan-out (ARCHITECTURE
//! §13/§14, issue #2). Owns the single connection to the relay and one Noise
//! session per dashboard. Broadcast frames (channels 1/2/3) are encrypted ONCE
//! PER DASHBOARD (pairwise — a single ciphertext can't decrypt for N keys) and
//! sent as opaque `Msg{to,data}`; inbound is decrypted per peer.
//!
//! Single-task design: all peer Noise state lives in this task, no sharing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header::AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::e2ee::{AuthorizedDevices, Handshake, Transport};
use crate::proto::Incoming;
use crate::relay_proto::{Envelope, Role, CONTROLLER_PEER};

pub struct RelayConfig {
    pub url: String,
    pub relay_token: Option<String>,
    pub room: String,
    pub psk: [u8; 32],
    pub static_priv: Vec<u8>,
    pub authz_path: PathBuf,
    /// Enrollment window: when true, an unknown device's static key is added
    /// (TOFU-under-PSK). When false (steady state), only authorized keys connect
    /// — this is what makes revocation stick (a removed key can't re-enroll).
    pub allow_enroll: bool,
    /// The `hello` frame (capabilities) sent to each dashboard once its E2EE is up.
    pub hello: String,
}

enum Peer {
    Handshaking(Box<Handshake>),
    Ready(Box<Transport>),
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub async fn run(
    cfg: RelayConfig,
    mut hi_rx: mpsc::Receiver<String>,
    mut lo_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<Incoming>,
    cancel: CancellationToken,
) {
    let mut backoff = 500u64;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = connect_once(&cfg, &mut hi_rx, &mut lo_rx, &in_tx, &cancel).await {
            tracing::warn!(error = %e, "relay session ended; reconnecting");
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(backoff)) => {}
        }
        backoff = (backoff * 2).min(15_000);
    }
}

async fn connect_once(
    cfg: &RelayConfig,
    hi_rx: &mut mpsc::Receiver<String>,
    lo_rx: &mut mpsc::Receiver<String>,
    in_tx: &mpsc::Sender<Incoming>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let mut req = cfg.url.as_str().into_client_request()?;
    if let Some(t) = &cfg.relay_token {
        req.headers_mut().insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}"))?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut sink, mut stream) = ws.split();

    // Join as the controller.
    let join = Envelope::Join {
        room: cfg.room.clone(),
        role: Role::Controller,
        peer: CONTROLLER_PEER.into(),
        token: cfg.relay_token.clone(),
    };
    sink.send(Message::Text(join.to_json())).await?;
    tracing::info!(room = %cfg.room, "relay: joined as controller");

    let mut authz = AuthorizedDevices::load(&cfg.authz_path)?;
    let mut peers: HashMap<String, Peer> = HashMap::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            hi = hi_rx.recv() => match hi {
                Some(text) => fanout(&mut sink, &mut peers, text.as_bytes()).await?,
                None => return Ok(()),
            },
            lo = lo_rx.recv() => match lo {
                Some(text) => fanout(&mut sink, &mut peers, text.as_bytes()).await?,
                None => return Ok(()),
            },
            inbound = stream.next() => {
                let text = match inbound {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.into()),
                };
                let env: Envelope = match serde_json::from_str(&text) { Ok(e) => e, Err(_) => continue };
                handle(env, cfg, &mut sink, &mut peers, &mut authz, in_tx).await?;
            }
        }
    }
}

/// Encrypt `plaintext` once per ready dashboard and send each ciphertext.
async fn fanout<S>(sink: &mut S, peers: &mut HashMap<String, Peer>, plaintext: &[u8]) -> anyhow::Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    for (peer, st) in peers.iter_mut() {
        if let Peer::Ready(t) = st {
            if let Ok(ct) = t.encrypt(plaintext) {
                let env = Envelope::Msg { to: peer.clone(), data: B64.encode(&ct) };
                sink.send(Message::Text(env.to_json())).await?;
            }
        }
    }
    Ok(())
}

async fn handle<S>(
    env: Envelope,
    cfg: &RelayConfig,
    sink: &mut S,
    peers: &mut HashMap<String, Peer>,
    authz: &mut AuthorizedDevices,
    in_tx: &mpsc::Sender<Incoming>,
) -> anyhow::Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    match env {
        Envelope::PeerJoin { peer } => {
            // Dashboard initiates the XX handshake; we wait as responder.
            match Handshake::responder(&cfg.static_priv, &cfg.psk) {
                Ok(hs) => {
                    peers.insert(peer.clone(), Peer::Handshaking(Box::new(hs)));
                    tracing::info!(%peer, "relay: dashboard joined; awaiting handshake");
                }
                Err(e) => tracing::warn!(%peer, error=%e, "responder init failed"),
            }
        }
        Envelope::PeerLeave { peer } => {
            peers.remove(&peer);
            tracing::info!(%peer, "relay: dashboard left");
        }
        Envelope::Deliver { from, data } => {
            let bytes = match B64.decode(&data) { Ok(b) => b, Err(_) => return Ok(()) };
            if let Some(peer) = peers.remove(&from) {
                match peer {
                    Peer::Handshaking(mut hs) => {
                        if hs.read(&bytes).is_err() {
                            tracing::warn!(%from, "handshake read failed; dropping peer");
                            return Ok(());
                        }
                        if hs.is_finished() {
                            let remote = hs.remote_static().unwrap_or_default();
                            let authorized = authz.contains(&remote)
                                || (cfg.allow_enroll && authz.add(&remote, &from, now()).is_ok());
                            if authorized {
                                match (*hs).into_transport() {
                                    Ok(mut t) => {
                                        // Greet the fresh dashboard with capabilities (§16.3 ADP-4).
                                        if let Ok(ct) = t.encrypt(cfg.hello.as_bytes()) {
                                            let e = Envelope::Msg { to: from.clone(), data: B64.encode(&ct) };
                                            sink.send(Message::Text(e.to_json())).await?;
                                        }
                                        peers.insert(from.clone(), Peer::Ready(Box::new(t)));
                                        tracing::info!(%from, "relay: dashboard authenticated (E2EE up); sent hello");
                                    }
                                    Err(e) => tracing::warn!(%from, error=%e, "transport init failed"),
                                }
                            } else {
                                tracing::warn!(%from, "relay: unauthorized device key (revoked or enrollment off); rejected");
                            }
                        } else {
                            // write the next handshake message back
                            match hs.write() {
                                Ok(m) => {
                                    let e = Envelope::Msg { to: from.clone(), data: B64.encode(&m) };
                                    sink.send(Message::Text(e.to_json())).await?;
                                    peers.insert(from, Peer::Handshaking(hs));
                                }
                                Err(e) => tracing::warn!(%from, error=%e, "handshake write failed"),
                            }
                        }
                    }
                    Peer::Ready(mut t) => {
                        if let Ok(pt) = t.decrypt(&bytes) {
                            if let Ok(inc) = serde_json::from_slice::<Incoming>(&pt) {
                                let _ = in_tx.send(inc).await;
                            }
                        }
                        peers.insert(from, Peer::Ready(t));
                    }
                }
            }
        }
        Envelope::Error { msg } => anyhow::bail!("relay error: {msg}"),
        _ => {}
    }
    Ok(())
}
