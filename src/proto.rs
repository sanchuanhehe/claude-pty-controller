//! Normalized wire schema (ARCHITECTURE §16.3 — normative).
//!
//! The dashboard is written against THIS schema, not Claude-native shapes.
//! Claude-native payloads ride inside `raw`. `v` gates breaking changes;
//! additive changes keep `v = 1` and consumers must ignore unknown fields.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTO_V: u32 = 1;

/// Steady-state status (channel 3). `notify` is a separate edge (see [`Outgoing::Notify`]),
/// NOT a state — round-end authority is the tab_status transition (§16.3 ADP-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Working,
    Waiting,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub transcript: bool,
    pub status: bool,
    pub multi_session: bool,
    pub input: bool,
}

/// Outbound messages (controller → dashboard). Internally tagged by `type`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    /// Capability handshake — sent on connect and on agent/session change (§16.3 ADP-4).
    Hello {
        v: u32,
        agent: String,
        capabilities: Capabilities,
    },
    /// Channel 1 — terminal screen bytes as a valid UTF-8 string.
    Output {
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        raw: String,
    },
    /// Channel 2 — one normalized transcript event (may be one of several from a JSONL row).
    Transcript {
        v: u32,
        agent: String,
        session: String,
        role: String,
        parts: Value,
        /// Dedup key: a JSONL row can expand to N events → (msg_uuid, part_index) (§16.3 ADP-7).
        msg_uuid: String,
        part_index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    /// Channel 3 — steady status.
    Event {
        v: u32,
        agent: String,
        session: String,
        state: State,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
    /// Channel 3 — one-shot notification edge (best-effort, e.g. bell).
    Notify {
        v: u32,
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Session switch / new session boundary (§3.2.1).
    Session {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        path: String,
        reason: String,
    },
}

/// Inbound messages (dashboard → controller). Internally tagged by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    /// Auth handshake — first frame on a direct connection.
    Auth { token: String },
    /// Submit text (adapter appends the agent's submit key, e.g. `\r`).
    Input { text: String },
    /// Raw control bytes written verbatim (Ctrl-C, arrows, …).
    Raw { text: String },
    /// Resize the PTY (→ `master.resize`, never an escape sequence).
    Resize { cols: u16, rows: u16 },
    /// Force a channel-2 re-read. `scope`: "tail" (incremental) | "full" (from 0).
    Refresh {
        #[serde(default)]
        scope: RefreshScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RefreshScope {
    #[default]
    Tail,
    Full,
}

impl Outgoing {
    pub fn output(session: Option<String>, raw: String) -> Self {
        Outgoing::Output { session, raw }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_roundtrips_and_omits_none_session() {
        let m = Outgoing::output(None, "\x1b[32mhi\x1b[0m".into());
        let s = m.to_json();
        assert!(s.contains("\"type\":\"output\""));
        assert!(!s.contains("session"));
    }

    #[test]
    fn incoming_parses_tagged_variants() {
        let i: Incoming = serde_json::from_str(r#"{"type":"input","text":"hi"}"#).unwrap();
        matches!(i, Incoming::Input { .. });
        let r: Incoming = serde_json::from_str(r#"{"type":"refresh"}"#).unwrap();
        match r {
            Incoming::Refresh { scope } => assert_eq!(scope, RefreshScope::Tail),
            _ => panic!("expected refresh"),
        }
        let rz: Incoming = serde_json::from_str(r#"{"type":"resize","cols":200,"rows":50}"#).unwrap();
        match rz {
            Incoming::Resize { cols, rows } => {
                assert_eq!(cols, 200);
                assert_eq!(rows, 50);
            }
            _ => panic!("expected resize"),
        }
    }

    #[test]
    fn event_state_serializes_lowercase() {
        let m = Outgoing::Event {
            v: PROTO_V,
            agent: "claude".into(),
            session: "s".into(),
            state: State::Working,
            detail: None,
        };
        assert!(m.to_json().contains("\"state\":\"working\""));
    }
}
