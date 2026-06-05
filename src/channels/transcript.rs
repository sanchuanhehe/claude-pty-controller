//! Channel 2 — JSONL tailing (ARCHITECTURE §3.2 / §3.2.1).
//!
//! Cursor advances by the LAST NEWLINE, never by file length, so a buffered
//! partial line (writes are ~100 ms flushed, no fsync) is never lost. Re-anchors
//! on `(path, inode)` identity change — a `len < offset` guard only catches
//! truncation, not an in-place/longer replacement (the /resume-to-another-file
//! case). Only lines that actually carry `sessionId` are session-identifying
//! (`file-history-snapshot` lines carry neither sessionId nor cwd).

use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Map a cwd to its `~/.claude/projects/<sanitized>` directory name.
/// Rule (verified v2.1.163): every non-alphanumeric byte → '-'; if the result
/// exceeds 200 bytes, a stable hash suffix is appended.
pub fn sanitize_cwd(cwd: &str) -> String {
    let mut s: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if s.len() > 200 {
        let h = djb2(cwd.as_bytes());
        s.truncate(200);
        s.push('-');
        s.push_str(&radix36(h));
    }
    s
}

fn djb2(bytes: &[u8]) -> u64 {
    let mut h: u64 = 5381;
    for &b in bytes {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

fn radix36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(D[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Extract the line's session id, if present. Returns `None` for line types that
/// carry no sessionId (e.g. `file-history-snapshot`) — callers must SKIP these for
/// switch detection, never treat absence as a new session (§3.2.1).
pub fn line_session_id(v: &Value) -> Option<&str> {
    v.get("sessionId").and_then(Value::as_str)
}

/// True for the message line types that carry a `uuid` (user/assistant/system/…).
/// Only these are forwarded on channel 2, so dashboard dedup-by-uuid works (§12.4).
pub fn has_uuid(v: &Value) -> bool {
    v.get("uuid").and_then(Value::as_str).is_some()
}

/// Incremental tailer for a single transcript file.
pub struct JsonlTailer {
    path: PathBuf,
    inode: u64,
    offset: u64,
}

impl JsonlTailer {
    /// Start tailing `path` from the given offset (0 = from start, replaying history).
    pub fn new(path: impl Into<PathBuf>, start_offset: u64) -> Self {
        let path = path.into();
        let inode = inode_of(&path).unwrap_or(0);
        Self { path, inode, offset: start_offset }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read whatever complete lines are now available; returns parsed JSON values.
    /// Lines that fail to parse (rare partial/garbage) are skipped, but the cursor
    /// only advances past the last newline so a partial final line is retried.
    pub fn poll(&mut self) -> std::io::Result<Vec<Value>> {
        // Re-anchor on identity change (replaced file / new session target).
        let cur_inode = inode_of(&self.path).unwrap_or(0);
        if cur_inode != self.inode {
            self.inode = cur_inode;
            self.offset = 0;
        }
        let len = match fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(Vec::new()),
        };
        if len < self.offset {
            self.offset = 0; // truncation
        }
        if len <= self.offset {
            return Ok(Vec::new());
        }
        let mut f = fs::File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;

        // Only consume up to the last newline; keep an unterminated tail for next poll.
        let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let consumed = &buf[..=last_nl];
        let mut out = Vec::new();
        for line in consumed.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                out.push(v);
            }
        }
        self.offset += (last_nl as u64) + 1;
        Ok(out)
    }
}

fn inode_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|m| m.ino())
    }
    #[cfg(not(unix))]
    {
        // Windows: fall back to (len,mtime) identity; inode-equivalent is future work (§15).
        fs::metadata(path).ok().map(|m| m.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sanitize_matches_observed_dirs() {
        assert_eq!(sanitize_cwd("/root/ws63-rs"), "-root-ws63-rs");
        assert_eq!(sanitize_cwd("/root/claude-pty-controller"), "-root-claude-pty-controller");
        // many-to-one collision is real and documented (§3.2)
        assert_eq!(sanitize_cwd("/root/ws63_rs"), sanitize_cwd("/root/ws63-rs"));
    }

    #[test]
    fn session_id_and_uuid_detection() {
        let msg: Value = serde_json::from_str(r#"{"type":"user","uuid":"u1","sessionId":"s1"}"#).unwrap();
        assert_eq!(line_session_id(&msg), Some("s1"));
        assert!(has_uuid(&msg));
        let snap: Value = serde_json::from_str(r#"{"type":"file-history-snapshot","snapshot":{}}"#).unwrap();
        assert_eq!(line_session_id(&snap), None);
        assert!(!has_uuid(&snap));
    }

    fn tmpfile(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cpc-test-{}-{}.jsonl", std::process::id(), name));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn newline_cursor_holds_back_partial_line() {
        let p = tmpfile("tail");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"uuid":"a","sessionId":"s"}}"#).unwrap();
        write!(f, r#"{{"uuid":"b","ses"#).unwrap(); // partial, no newline
        f.flush().unwrap();

        let mut t = JsonlTailer::new(&p, 0);
        let v = t.poll().unwrap();
        assert_eq!(v.len(), 1, "partial second line must be held back");
        assert_eq!(line_session_id(&v[0]), Some("s"));

        // complete the partial line
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"sionId":"s2"}}"#).unwrap();
        f.flush().unwrap();
        let v2 = t.poll().unwrap();
        assert_eq!(v2.len(), 1);
        assert_eq!(line_session_id(&v2[0]), Some("s2"));
        let _ = fs::remove_file(&p);
    }
}
