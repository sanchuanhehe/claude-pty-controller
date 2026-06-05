//! WebSocket transport (ARCHITECTURE §6/§7). One task owns the connection and
//! `select!`s between draining the outbound queue and reading inbound frames, so
//! there's no shared sink and no frame interleaving. Reconnect uses capped
//! exponential backoff; while disconnected the bounded `out_rx` is NOT drained,
//! so backpressure applies upstream (§7).
//!
//! NOTE: this is the v1 single-connection transport (direct / single dashboard).
//! The relay + per-dashboard E2EE fan-out is M4 (§13/§14, issues #1/#2).

use crate::proto::Incoming;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

pub async fn run(
    url: String,
    mut out_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<Incoming>,
    cancel: CancellationToken,
) {
    let mut backoff_ms = 500u64;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                tracing::info!(%url, "websocket connected");
                backoff_ms = 500;
                let (mut sink, mut stream) = ws.split();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        outbound = out_rx.recv() => match outbound {
                            Some(text) => {
                                if sink.send(Message::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            None => return, // producers gone
                        },
                        inbound = stream.next() => match inbound {
                            Some(Ok(Message::Text(t))) => {
                                if let Ok(msg) = serde_json::from_str::<Incoming>(&t) {
                                    let _ = in_tx.send(msg).await;
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "ws read error");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                tracing::warn!("websocket disconnected; will reconnect");
            }
            Err(e) => {
                tracing::warn!(error = %e, "websocket connect failed");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)) => {}
        }
        backoff_ms = (backoff_ms * 2).min(15_000);
    }
}
