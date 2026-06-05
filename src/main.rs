//! claude-pty-controller — entry point.
//!
//! See `docs/ARCHITECTURE.md` for the full design. This is the M1 skeleton:
//! task wiring lands incrementally per the milestones in §11.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("claude-pty-controller starting (skeleton)");

    // Refuse to start without a control token (ARCHITECTURE.md §5).
    if std::env::var("CONTROL_TOKEN").map_or(true, |t| t.is_empty()) {
        anyhow::bail!("CONTROL_TOKEN is required (see docs/ARCHITECTURE.md §5)");
    }

    // TODO(M1): spawn PTY, channel-1 reader, ws_outbound.
    // TODO(M2): inbound input/raw/resize, auth handshake, wss.
    // TODO(M3): JSONL tail (newline cursor), OSC state machine (&[u8]).
    // TODO(M4): bounded backpressure, reconnect backoff, graceful shutdown.

    Ok(())
}
