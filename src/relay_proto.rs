//! Relay envelope protocol (ARCHITECTURE §13). The outer endpoint↔relay link.
//! The relay routes by `room`/`peer` and never inspects `data` — which carries
//! opaque E2EE bytes (handshake or AEAD ciphertext, base64). The relay is a
//! zero-knowledge forwarder.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Controller,
    Dashboard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Envelope {
    /// First frame from an endpoint: register into a room.
    Join {
        room: String,
        role: Role,
        /// Peer id (controllers use a fixed id like "controller").
        peer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Ack of a successful join.
    Joined,
    /// Endpoint → relay: deliver `data` to peer `to` in my room.
    Msg { to: String, data: String },
    /// Relay → recipient: `data` from peer `from`.
    Deliver { from: String, data: String },
    /// Relay → controller: a dashboard joined / left (drives Noise session setup/teardown).
    PeerJoin { peer: String },
    PeerLeave { peer: String },
    /// Relay → endpoint: fatal error (then the relay closes the connection).
    Error { msg: String },
}

impl Envelope {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

pub const CONTROLLER_PEER: &str = "controller";
