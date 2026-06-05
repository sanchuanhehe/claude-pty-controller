//! claude-pty-controller — binary entry point.
//!
//! M1 skeleton (ARCHITECTURE §11): tmux session host + channel 1 (UTF-8 tail) +
//! channel 3 (OSC status) outbound over WebSocket, plus inbound input/raw/resize.
//! Channel 2 (transcript), auth, wss, relay/E2EE land in M2–M4.

use std::io::Read;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use claude_pty_controller::adapter::claude::{self, AGENT_ID};
use claude_pty_controller::channels::osc::OscParser;
use claude_pty_controller::channels::output::Utf8TailBuffer;
use claude_pty_controller::config::Config;
use claude_pty_controller::proto::{Capabilities, Incoming, Outgoing, PROTO_V};
use claude_pty_controller::pty::tmux::{TmuxConfig, TmuxHost};
use claude_pty_controller::ws;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    tracing::info!(remote = %cfg.remote_url, agent = ?cfg.agent_cmd, "starting claude-pty-controller (M1)");

    let cancel = CancellationToken::new();

    // SIGINT/SIGTERM → graceful detach (keep tmux session alive, §8).
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("signal received; detaching");
            cancel.cancel();
        });
    }

    // Session host.
    let (mut host, reader) = TmuxHost::spawn(TmuxConfig {
        socket: cfg.tmux_socket.clone(),
        session: cfg.tmux_session.clone(),
        agent_cmd: cfg.agent_cmd.clone(),
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        cols: cfg.cols,
        rows: cfg.rows,
    })?;

    // Channels (bounded — §7 backpressure).
    let (out_tx, out_rx) = mpsc::channel::<String>(1024);
    let (in_tx, mut in_rx) = mpsc::channel::<Incoming>(256);

    // Hello (§16.3 ADP-4).
    let hello = Outgoing::Hello {
        v: PROTO_V,
        agent: AGENT_ID.into(),
        capabilities: Capabilities { transcript: false, status: true, multi_session: false, input: true },
    };
    let _ = out_tx.send(hello.to_json()).await;

    // WebSocket task.
    tokio::spawn(ws::run(cfg.remote_url.clone(), out_rx, in_tx, cancel.clone()));

    // PTY reader (blocking) → channel 1 (output) + channel 3 (osc).
    {
        let out_tx = out_tx.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            let mut utf8 = Utf8TailBuffer::new();
            let mut osc = OscParser::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        let text = utf8.push(chunk);
                        if !text.is_empty() {
                            // drop on full rather than stall the PTY reader (§7).
                            let _ = out_tx.try_send(Outgoing::output(None, text).to_json());
                        }
                        for ev in osc.feed(chunk) {
                            if let Some(msg) = claude::osc_to_outgoing(&ev, None) {
                                let _ = out_tx.try_send(msg.to_json());
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            tracing::info!("pty reader ended (child exit/detach)");
            cancel.cancel();
        });
    }

    // pty_writer / inbound loop (single writer, §2).
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = in_rx.recv() => match msg {
                Some(Incoming::Input { text }) => { let _ = host.write(&claude::encode_submit(&text)); }
                Some(Incoming::Raw { text }) => { let _ = host.write(text.as_bytes()); }
                Some(Incoming::Resize { cols, rows }) => {
                    if let Err(e) = host.resize(cols, rows) { tracing::warn!(error=%e, "resize"); }
                }
                Some(Incoming::Refresh { .. }) => { /* channel-2 not wired in M1 */ }
                Some(Incoming::Auth { .. }) => { /* token check lands with wss/auth in M2 */ }
                None => break,
            }
        }
    }

    // Graceful shutdown: detach (keep session + claude alive, §8).
    if host.session_alive() {
        host.detach();
    }
    tracing::info!("shutdown");
    Ok(())
}
