//! The three data channels (ARCHITECTURE §3). Channel 1 (output) is
//! agent-agnostic; channels 2 (transcript) and 3 (osc/status) are the
//! Claude-specific bits that the `AgentAdapter` (§16) owns.

pub mod osc;
pub mod output;
pub mod transcript;
