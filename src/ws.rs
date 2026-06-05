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
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header::AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// Build the handshake request, attaching `Authorization: Bearer <token>` (§5).
/// `wss://` URLs go over rustls (no OpenSSL); `connect_async` selects TLS by scheme.
fn build_request(url: &str, token: Option<&str>) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut req = url.into_client_request()?;
    if let Some(t) = token {
        req.headers_mut().insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}"))?);
    }
    Ok(req)
}

pub async fn run(
    url: String,
    token: Option<String>,
    mut out_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<Incoming>,
    cancel: CancellationToken,
) {
    let mut backoff_ms = 500u64;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let req = match build_request(&url, token.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "bad REMOTE_URL/token; not retrying");
                return;
            }
        };
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => {
                tracing::info!(%url, tls = url.starts_with("wss://"), "websocket connected");
                backoff_ms = 500;
                let (mut sink, mut stream) = ws.split();
                // Authenticate first (also as a frame, for frame-routing relays).
                if let Some(t) = &token {
                    let auth = json!({"type": "auth", "token": t}).to_string();
                    if sink.send(Message::Text(auth)).await.is_err() {
                        continue;
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_carries_bearer_token() {
        let req = build_request("wss://relay.example:9000/", Some("sekret")).unwrap();
        assert_eq!(req.headers().get("Authorization").unwrap(), "Bearer sekret");
    }

    #[test]
    fn request_without_token_has_no_auth_header() {
        let req = build_request("ws://127.0.0.1:9000/", None).unwrap();
        assert!(req.headers().get("Authorization").is_none());
    }
}
