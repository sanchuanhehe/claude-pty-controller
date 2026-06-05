//! claude-pty-controller — library crate (testable cores + wiring blocks).
//! The binary (`main.rs`) composes these. See `docs/ARCHITECTURE.md`.

pub mod adapter;
pub mod channels;
pub mod config;
pub mod e2ee;
pub mod proto;
pub mod pty;
pub mod relay_client;
pub mod relay_proto;
pub mod session;
pub mod singleton;
pub mod watchdog;
pub mod ws;
