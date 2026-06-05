//! Configuration (ARCHITECTURE §5/§9). v1 reads env vars; a consolidated config
//! schema with flag precedence is tracked in issue #6 (§12 OPS-8).

use anyhow::{bail, Result};

pub struct Config {
    pub remote_url: String,
    /// Auth/pairing token. Required unless the endpoint is loopback + `--insecure`.
    pub control_token: Option<String>,
    pub insecure: bool,
    pub agent_cmd: Vec<String>,
    pub tmux_socket: String,
    pub tmux_session: String,
    pub cols: u16,
    pub rows: u16,
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
        // §5: refuse to start without a token unless explicitly insecure on loopback.
        if control_token.is_none() && !(insecure && is_loopback) {
            bail!("CONTROL_TOKEN is required (set it, or use CPC_INSECURE=1 on a loopback REMOTE_URL). See docs/ARCHITECTURE.md §5.");
        }

        Ok(Self {
            remote_url,
            control_token,
            insecure,
            agent_cmd,
            tmux_socket: std::env::var("TMUX_SOCKET").unwrap_or_else(|_| "claude-ctl".into()),
            tmux_session: std::env::var("TMUX_SESSION").unwrap_or_else(|_| "claude-ctl".into()),
            cols: parse_u16("COLS", 160),
            rows: parse_u16("ROWS", 45),
        })
    }
}

fn parse_u16(key: &str, default: u16) -> u16 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
