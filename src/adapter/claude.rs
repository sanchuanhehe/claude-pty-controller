//! `ClaudeAdapter` — the first adapter (ARCHITECTURE §16.5). v1 covers the
//! OSC→state mapping and input encoding; transcript discovery/parse lands with
//! the multi-session work.

use crate::channels::osc::OscEvent;
use crate::proto::{Outgoing, State, PROTO_V};

pub const AGENT_ID: &str = "claude";

/// Map a claude tab_status string to a normalized state. Matched by PREFIX, not
/// exact literal, so the `Working…` ellipsis (U+2026) and minor version drift
/// don't break it (§3.3 robustness).
pub fn state_from_status(status: &str) -> Option<State> {
    let s = status.trim();
    if s.starts_with("Working") {
        Some(State::Working)
    } else if s.starts_with("Waiting") {
        Some(State::Waiting)
    } else if s.starts_with("Idle") {
        Some(State::Idle)
    } else {
        None
    }
}

/// Convert an OSC event into a normalized outbound message (channel 3).
/// `notify` (bell) is a one-shot edge, NOT a state (§16.3 ADP-6). Progress/title
/// are not surfaced as states in v1.
pub fn osc_to_outgoing(ev: &OscEvent, session: Option<&str>) -> Option<Outgoing> {
    match ev {
        OscEvent::TabStatus { status, .. } => {
            let state = state_from_status(status.as_deref()?)?;
            Some(Outgoing::Event {
                v: PROTO_V,
                agent: AGENT_ID.into(),
                session: session.unwrap_or("").into(),
                state,
                detail: None,
            })
        }
        OscEvent::Bell => Some(Outgoing::Notify {
            v: PROTO_V,
            agent: AGENT_ID.into(),
            session: session.map(str::to_owned),
        }),
        OscEvent::Progress { .. } | OscEvent::Title { .. } => None,
    }
}

/// Encode a "submit text" into PTY bytes (claude submits on CR). The input side
/// of the adapter (ARCHITECTURE §16.2 ADP-1); other agents override this.
pub fn encode_submit(text: &str) -> Vec<u8> {
    let mut v = text.as_bytes().to_vec();
    v.push(b'\r');
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_prefix_matching() {
        assert_eq!(state_from_status("Working…"), Some(State::Working));
        assert_eq!(state_from_status("Idle"), Some(State::Idle));
        assert_eq!(state_from_status("Waiting"), Some(State::Waiting));
        assert_eq!(state_from_status("generating"), None);
    }

    #[test]
    fn bell_maps_to_notify_not_state() {
        let m = osc_to_outgoing(&OscEvent::Bell, Some("s")).unwrap();
        assert!(matches!(m, Outgoing::Notify { .. }));
    }
}
