//! Channel-2 driver (ARCHITECTURE §3.2 / §3.2.1) — discovers the active
//! transcript file, locks onto it, follows session switches, and forwards
//! normalized transcript messages. Wired to three refresh sources (poll tick,
//! channel-3 turn-end, manual refresh) by the caller.

use std::path::PathBuf;

use crate::adapter::claude;
use crate::channels::transcript::{
    find_active_jsonl, line_session_id, JsonlTailer,
};
use crate::proto::Outgoing;

pub struct TranscriptWatcher {
    project_dir: PathBuf,
    tailer: Option<JsonlTailer>,
    current_path: Option<PathBuf>,
    current_sid: Option<String>,
}

impl TranscriptWatcher {
    pub fn new(project_dir: PathBuf) -> Self {
        Self { project_dir, tailer: None, current_path: None, current_sid: None }
    }

    /// Run one pass. `full` resets the cursor to 0 (manual `refresh:full`).
    /// Returns the outbound messages to send (a `Session` boundary on switch,
    /// then one `Transcript` per uuid-bearing row).
    pub fn poll(&mut self, full: bool) -> Vec<Outgoing> {
        let mut out = Vec::new();

        // (Re)discover the active file each pass; re-lock when it changes.
        let active = find_active_jsonl(&self.project_dir);
        let switched = match (&active, &self.current_path) {
            (Some(a), Some(c)) => a != c,
            (Some(_), None) => true,
            _ => false,
        };
        if let Some(active) = active {
            if switched || self.tailer.is_none() {
                self.current_path = Some(active.clone());
                self.tailer = Some(JsonlTailer::new(active, 0));
                self.current_sid = None; // learned from the first sessionId-bearing line
            }
        } else {
            return out; // no transcript yet
        }

        if full {
            if let Some(path) = &self.current_path {
                self.tailer = Some(JsonlTailer::new(path.clone(), 0));
            }
        }

        let Some(tailer) = self.tailer.as_mut() else { return out };
        let path = self.current_path.clone().unwrap_or_default();
        let values = tailer.poll().unwrap_or_default();

        for v in values {
            // Track session identity from lines that carry it (skip the rest, §3.2.1).
            if let Some(sid) = line_session_id(&v) {
                if self.current_sid.as_deref() != Some(sid) {
                    let reason = if self.current_sid.is_none() { "new" } else { "switch" };
                    self.current_sid = Some(sid.to_string());
                    out.push(Outgoing::Session {
                        session_id: sid.to_string(),
                        cwd: None, // recovered from sessions/*.json in multi-session (§3.5)
                        path: path.to_string_lossy().into_owned(),
                        reason: reason.into(),
                    });
                }
            }
            // Forward only uuid-bearing message rows (§3.2 / §12.4).
            if let Some(msg) = claude::parse_transcript_line(&v) {
                out.push(msg);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmpdir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cpc-sess-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn locks_active_file_emits_session_then_transcripts() {
        let dir = tmpdir("watch");
        let sid = "12345678-1234-1234-1234-123456789abc";
        let f = dir.join(format!("{sid}.jsonl"));
        let mut fh = fs::File::create(&f).unwrap();
        writeln!(
            fh,
            r#"{{"type":"user","uuid":"u1","sessionId":"{sid}","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        // an auxiliary line with no uuid — must NOT be forwarded
        writeln!(fh, r#"{{"type":"file-history-snapshot","snapshot":{{}}}}"#).unwrap();
        fh.flush().unwrap();

        let mut w = TranscriptWatcher::new(dir.clone());
        let msgs = w.poll(false);
        // expect: one Session boundary + one Transcript (user row), snapshot skipped
        assert!(matches!(msgs.first(), Some(Outgoing::Session { .. })));
        let transcripts = msgs.iter().filter(|m| matches!(m, Outgoing::Transcript { .. })).count();
        assert_eq!(transcripts, 1);

        // second poll, no new lines → nothing
        assert!(w.poll(false).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
