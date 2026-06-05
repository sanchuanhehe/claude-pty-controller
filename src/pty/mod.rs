//! Session host (ARCHITECTURE §2.1 / §15). `TmuxHost` is the Unix impl of the
//! "session host" role (persistence + bidirectional attach); a native-Windows
//! `ConPtyHost` is future work (§15).

pub mod tmux;

pub use tmux::TmuxHost;
