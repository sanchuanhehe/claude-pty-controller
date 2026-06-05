//! Configuration (ARCHITECTURE §5/§9). v1 reads env vars; a consolidated config
//! schema with flag precedence is tracked in issue #6 (§12 OPS-8).

use anyhow::{bail, Result};
use std::path::PathBuf;

pub struct Config {
    pub remote_url: String,
    /// Direct-mode bearer token. Required unless loopback + insecure, or relay mode.
    pub control_token: Option<String>,
    pub insecure: bool,
    pub agent_cmd: Vec<String>,
    pub tmux_socket: String,
    pub tmux_session: String,
    pub cols: u16,
    pub rows: u16,
    // Relay + E2EE (§13/§14). Present `pairing_secret` ⇒ relay/E2EE mode.
    pub pairing_secret: Option<String>,
    pub relay_token: Option<String>,
    pub allow_enroll: bool,
    pub home: PathBuf,
}

impl Config {
    pub fn relay_mode(&self) -> bool {
        self.pairing_secret.is_some()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let remote_url = std::env::var("REMOTE_URL").unwrap_or_else(|_| "ws://127.0.0.1:9000".into());
        let control_token = std::env::var("CONTROL_TOKEN").ok().filter(|s| !s.is_empty());
        let insecure = std::env::var("CPC_INSECURE").map(|v| v == "1").unwrap_or(false);
        let agent_cmd = std::env::var("AGENT_CMD")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["claude".into()]);

        let is_loopback = remote_url.contains("127.0.0.1") || remote_url.contains("localhost") || remote_url.contains("[::1]");
        let is_wss = remote_url.starts_with("wss://");
        let is_ws = remote_url.starts_with("ws://");
        if !is_wss && !is_ws {
            bail!("REMOTE_URL must be ws:// or wss:// (got {remote_url})");
        }
        // §5: production requires wss; plaintext ws:// only on loopback + insecure.
        if is_ws && !(insecure && is_loopback) {
            bail!("plaintext ws:// is only allowed on loopback with CPC_INSECURE=1; use wss:// in production. See docs/ARCHITECTURE.md §5.");
        }
        let pairing_secret = std::env::var("PAIRING_SECRET").ok().filter(|s| !s.is_empty());
        if let Some(s) = &pairing_secret {
            // #1: enforce high entropy (Noise+PSK is not a PAKE).
            crate::e2ee::validate_pairing_secret(s)?;
        }

        // §5: need SOME auth — direct CONTROL_TOKEN, or relay-mode PAIRING_SECRET,
        // or explicit insecure on loopback.
        if control_token.is_none() && pairing_secret.is_none() && !(insecure && is_loopback) {
            bail!("set CONTROL_TOKEN (direct) or PAIRING_SECRET (relay/E2EE), or use CPC_INSECURE=1 on loopback. See docs/ARCHITECTURE.md §5.");
        }

        let home = std::env::var_os("CLAUDE_PTY_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude-pty-controller")))
            .unwrap_or_else(|| PathBuf::from(".claude-pty-controller"));

        Ok(Self {
            remote_url,
            control_token,
            insecure,
            agent_cmd,
            tmux_socket: std::env::var("TMUX_SOCKET").unwrap_or_else(|_| "claude-ctl".into()),
            tmux_session: std::env::var("TMUX_SESSION").unwrap_or_else(|_| "claude-ctl".into()),
            cols: parse_u16("COLS", 160),
            rows: parse_u16("ROWS", 45),
            pairing_secret,
            relay_token: std::env::var("RELAY_TOKEN").ok().filter(|s| !s.is_empty()),
            allow_enroll: std::env::var("CPC_ALLOW_ENROLL").map(|v| v == "1").unwrap_or(false),
            home,
        })
    }
}

fn parse_u16(key: &str, default: u16) -> u16 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
