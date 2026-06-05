//! `TmuxHost` — runs claude inside a reconnectable tmux session (ARCHITECTURE §2.1).
//!
//! `tmux -L <sock> new-session -A -s <name> … <agent>` gives background
//! persistence + local `tmux attach` bidirectional sharing. We set
//! `allow-passthrough all` (pass claude's DCS-wrapped OSC through even when the
//! controller window isn't current), `status off` / `pane-border-status off`
//! (keep tmux chrome + the per-second clock out of channel 1), and own the
//! geometry via `master.resize()` on reconnect (the `-x/-y` flags are ignored on
//! takeover of an existing session).

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::process::Command;

pub struct TmuxConfig {
    pub socket: String,
    pub session: String,
    pub agent_cmd: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            socket: "claude-ctl".into(),
            session: "claude-ctl".into(),
            agent_cmd: vec!["claude".into()],
            cwd: std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into()),
            cols: 160,
            rows: 45,
        }
    }
}

pub struct TmuxHost {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    cfg: TmuxConfig,
}

impl TmuxHost {
    /// Spawn the tmux client in a PTY and return the host plus a blocking reader.
    pub fn spawn(cfg: TmuxConfig) -> Result<(Self, Box<dyn Read + Send>)> {
        let pair = native_pty_system()
            .openpty(PtySize { rows: cfg.rows, cols: cfg.cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new("tmux");
        cmd.args([
            "-L", &cfg.socket,
            "new-session", "-A",
            "-s", &cfg.session,
            "-x", &cfg.cols.to_string(),
            "-y", &cfg.rows.to_string(),
        ]);
        cmd.args(&cfg.agent_cmd);
        cmd.cwd(&cfg.cwd);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).context("spawn tmux")?;
        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let host = TmuxHost { master: pair.master, writer, child, cfg };
        host.apply_options_async();
        Ok((host, reader))
    }

    /// Apply server options once the session exists (retries; best-effort).
    fn apply_options_async(&self) {
        let socket = self.cfg.socket.clone();
        std::thread::spawn(move || {
            for _ in 0..50 {
                let ok = run_tmux(&socket, &["set", "-g", "allow-passthrough", "all"])
                    && run_tmux(&socket, &["set", "-g", "status", "off"])
                    && run_tmux(&socket, &["set", "-g", "pane-border-status", "off"])
                    && run_tmux(&socket, &["set", "-g", "window-size", "latest"]);
                if ok {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            tracing::warn!("could not apply tmux options (server not ready?)");
        });
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resize")
    }

    /// True iff the tmux session still exists (distinguishes detach from kill, §8).
    pub fn session_alive(&self) -> bool {
        run_tmux(&self.cfg.socket, &["has-session", "-t", &self.cfg.session])
    }

    /// Detach the controller's client (keeps the session + claude alive, §8).
    pub fn detach(&self) {
        let _ = run_tmux(&self.cfg.socket, &["detach-client", "-s", &self.cfg.session]);
    }

    pub fn try_wait_done(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

fn run_tmux(socket: &str, args: &[&str]) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
