//! Adapter layer (ARCHITECTURE §16). Channel 1 is agent-agnostic; channels 2/3
//! and input encoding are the agent-specific parts. v1 ships `ClaudeAdapter`;
//! the full `AgentAdapter` trait + a second adapter are M6.

pub mod claude;
