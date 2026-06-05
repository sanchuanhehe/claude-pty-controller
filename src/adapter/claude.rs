//! `ClaudeAdapter` — the first adapter (ARCHITECTURE §16.5). v1 covers the
//! OSC→state mapping and input encoding; transcript discovery/parse lands with
//! the multi-session work.

use crate::channels::osc::OscEvent;
use crate::proto::{Outgoing, State, PROTO_V};
use serde_json::{json, Value};

pub const AGENT_ID: &str = "claude";

/// Parse one Claude JSONL row into a normalized `transcript` message (§16.3).
/// Returns `None` for rows we don't forward: those without a `uuid` (auxiliary
/// line types — see §3.2; keeps dashboard dedup-by-uuid sound) or without a
/// message body. Content blocks (text / thinking / tool_use / tool_result) map
/// to normalized `parts`; the original row is kept in `raw` for fidelity.
pub fn parse_transcript_line(v: &Value) -> Option<Outgoing> {
    let uuid = v.get("uuid").and_then(Value::as_str)?;
    let session = v.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let message = v.get("message")?;
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| v.get("type").and_then(Value::as_str))
        .unwrap_or("assistant")
        .to_string();

    let parts = match message.get("content") {
        Some(Value::String(s)) => vec![json!({"kind": "text", "text": s})],
        Some(Value::Array(blocks)) => blocks.iter().filter_map(block_to_part).collect(),
        _ => return None,
    };

    Some(Outgoing::Transcript {
        v: PROTO_V,
        agent: AGENT_ID.into(),
        session,
        role,
        parts: Value::Array(parts),
        msg_uuid: uuid.to_string(),
        part_index: 0,
        raw: Some(v.clone()),
    })
}

fn block_to_part(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(json!({"kind": "text", "text": block.get("text").cloned().unwrap_or_default()})),
        // Claude stores reasoning under the `thinking` key (§16.3 ADP-2).
        "thinking" => Some(json!({"kind": "thinking", "text": block.get("thinking").cloned().unwrap_or_default()})),
        "tool_use" => Some(json!({
            "kind": "tool_use",
            "id": block.get("id").cloned().unwrap_or_default(),
            "name": block.get("name").cloned().unwrap_or_default(),
            "input": block.get("input").cloned().unwrap_or(Value::Null),
        })),
        // content may be a string or an array of blocks (incl. images) — pass through (ADP-3).
        "tool_result" => Some(json!({
            "kind": "tool_result",
            "forId": block.get("tool_use_id").cloned().unwrap_or_default(),
            "content": block.get("content").cloned().unwrap_or(Value::Null),
        })),
        _ => None, // unknown block kinds survive in `raw`
    }
}

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

    #[test]
    fn parse_assistant_row_with_thinking_and_tool_use() {
        let line: Value = serde_json::from_str(
            r#"{"type":"assistant","uuid":"u1","sessionId":"s1","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"ok"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}
            ]}}"#,
        )
        .unwrap();
        let m = parse_transcript_line(&line).unwrap();
        match m {
            Outgoing::Transcript { session, role, parts, msg_uuid, .. } => {
                assert_eq!(session, "s1");
                assert_eq!(role, "assistant");
                assert_eq!(msg_uuid, "u1");
                let arr = parts.as_array().unwrap();
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0]["kind"], "thinking");
                assert_eq!(arr[0]["text"], "hmm");
                assert_eq!(arr[2]["kind"], "tool_use");
                assert_eq!(arr[2]["name"], "Bash");
            }
            _ => panic!("expected transcript"),
        }
    }

    #[test]
    fn auxiliary_line_without_uuid_is_skipped() {
        let line: Value =
            serde_json::from_str(r#"{"type":"file-history-snapshot","snapshot":{}}"#).unwrap();
        assert!(parse_transcript_line(&line).is_none());
    }
}
