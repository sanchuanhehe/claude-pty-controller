//! Channel 3 — OSC/DCS state machine (ARCHITECTURE §3.3).
//!
//! Operates on `&[u8]` (never `byte as char`) so byte-index slicing can't panic
//! on multibyte fields. Handles:
//!   - OSC 21337 tab status (indicator / status), OSC 9;4 progress, OSC 0/2 title, BEL
//!   - both terminators: BEL (0x07) and ST (ESC \)
//!   - tmux DCS passthrough (`ESC P tmux ; <esc-doubled> ST`) and GNU screen
//!     (`ESC P <raw> ST`, no doubling) — unwrapped and re-fed to the OSC parser
//!   - state preserved across 8 KiB chunk boundaries
//!
//! Robustness (§3.3): status is matched by caller on indicator/prefix, not exact
//! ellipsis; only 21337/9;4/0/2 are recognized, other programs' OSC are ignored;
//! CSI (`ESC [ …`) returns to Ground without entering OSC.

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 21337 — fields present in the sequence (absent = cleared).
    TabStatus {
        status: Option<String>,
        indicator: Option<String>,
    },
    /// OSC 9;4 — iTerm2 progress.
    Progress { operation: String, percentage: Option<u32> },
    /// OSC 0 / OSC 2 — window title.
    Title { title: String },
    /// Standalone BEL.
    Bell,
}

enum St {
    Ground,
    Esc,
    /// Inside `ESC ] …` — accumulating the OSC body (after the `]`).
    Osc,
    /// Inside `ESC P …` — accumulating the DCS body (after the `P`).
    Dcs,
}

pub struct OscParser {
    state: St,
    buf: Vec<u8>,
    /// Recursion guard for re-fed DCS-unwrapped content.
    depth: u8,
}

impl Default for OscParser {
    fn default() -> Self {
        Self { state: St::Ground, buf: Vec::new(), depth: 0 }
    }
}

impl OscParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &b in data {
            self.step(b, &mut events);
        }
        events
    }

    fn step(&mut self, b: u8, events: &mut Vec<OscEvent>) {
        match self.state {
            St::Ground => {
                if b == ESC {
                    self.state = St::Esc;
                } else if b == BEL {
                    events.push(OscEvent::Bell);
                }
            }
            St::Esc => match b {
                b']' => {
                    self.state = St::Osc;
                    self.buf.clear();
                }
                b'P' => {
                    self.state = St::Dcs;
                    self.buf.clear();
                }
                ESC => { /* stay in Esc */ }
                // CSI (`[`) and everything else: not an OSC/DCS — back to Ground.
                _ => self.state = St::Ground,
            },
            St::Osc => {
                if b == BEL {
                    self.finish_osc(events);
                } else if b == ESC {
                    // Possible ST start; if next byte is `\`, terminator.
                    self.buf.push(ESC);
                } else if b == b'\\' && self.buf.last() == Some(&ESC) {
                    self.buf.pop(); // drop the ESC of ST
                    self.finish_osc(events);
                } else {
                    self.buf.push(b);
                }
            }
            St::Dcs => {
                if b == b'\\' && self.buf.last() == Some(&ESC) {
                    self.buf.pop();
                    self.finish_dcs(events);
                } else {
                    self.buf.push(b);
                }
            }
        }
    }

    fn finish_osc(&mut self, events: &mut Vec<OscEvent>) {
        let body = std::mem::take(&mut self.buf);
        self.state = St::Ground;
        parse_osc_body(&body, events);
    }

    fn finish_dcs(&mut self, events: &mut Vec<OscEvent>) {
        let body = std::mem::take(&mut self.buf);
        self.state = St::Ground;
        if self.depth >= 2 {
            return; // recursion guard
        }
        let inner: Vec<u8> = if let Some(rest) = body.strip_prefix(b"tmux;") {
            // tmux: un-double ESC.
            undouble_esc(rest)
        } else {
            // GNU screen passthrough (or unknown): feed raw.
            body
        };
        let mut sub = OscParser { depth: self.depth + 1, ..Default::default() };
        events.extend(sub.feed(&inner));
    }
}

fn undouble_esc(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == ESC && input.get(i + 1) == Some(&ESC) {
            out.push(ESC);
            i += 2;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Parse an OSC body (the bytes after `ESC ]`, terminator already stripped).
fn parse_osc_body(body: &[u8], events: &mut Vec<OscEvent>) {
    let (cmd_bytes, data) = match body.iter().position(|&c| c == b';') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &body[body.len()..]),
    };
    let cmd: u32 = std::str::from_utf8(cmd_bytes).ok().and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
    match cmd {
        0 | 2 => events.push(OscEvent::Title { title: lossy(data) }),
        9 => parse_iterm2(data, events),
        21337 => parse_tab_status(data, events),
        _ => {} // ignore other programs' OSC
    }
}

fn parse_iterm2(data: &[u8], events: &mut Vec<OscEvent>) {
    let mut parts = data.split(|&c| c == b';');
    let sub = parts.next().and_then(|p| std::str::from_utf8(p).ok()).unwrap_or("");
    if sub == "4" {
        let op = parts.next().and_then(|p| std::str::from_utf8(p).ok()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        let pct = parts.next().and_then(|p| std::str::from_utf8(p).ok()).and_then(|s| s.parse::<u32>().ok());
        let operation = match op {
            0 => "clear",
            1 => "set",
            2 => "error",
            3 => "indeterminate",
            _ => "unknown",
        }
        .to_string();
        events.push(OscEvent::Progress { operation, percentage: pct });
    }
}

fn parse_tab_status(data: &[u8], events: &mut Vec<OscEvent>) {
    let mut status = None;
    let mut indicator = None;
    for pair in data.split(|&c| c == b';') {
        if let Some(eq) = pair.iter().position(|&c| c == b'=') {
            let key = &pair[..eq];
            let val = &pair[eq + 1..];
            match key {
                b"status" => status = Some(unescape_status(val)),
                b"indicator" => indicator = Some(lossy(val)),
                _ => {}
            }
        }
    }
    events.push(OscEvent::TabStatus { status, indicator });
}

/// Undo claude's `\;` / `\\` escaping in the status field.
fn unescape_status(val: &[u8]) -> String {
    let s = lossy(val);
    s.replace("\\;", ";").replace("\\\\", "\\")
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(p: &mut OscParser, bytes: &[u8]) -> Vec<OscEvent> {
        p.feed(bytes)
    }

    #[test]
    fn tab_status_bel_terminated_with_ellipsis() {
        let mut p = OscParser::new();
        let ev = one(&mut p, "\x1b]21337;indicator=#00cc00;status=Working\u{2026};status-color=#fff\x07".as_bytes());
        assert_eq!(
            ev,
            vec![OscEvent::TabStatus {
                status: Some("Working…".into()),
                indicator: Some("#00cc00".into())
            }]
        );
    }

    #[test]
    fn tab_status_st_terminated() {
        let mut p = OscParser::new();
        let ev = one(&mut p, b"\x1b]21337;status=Idle\x1b\\");
        assert_eq!(ev, vec![OscEvent::TabStatus { status: Some("Idle".into()), indicator: None }]);
    }

    #[test]
    fn progress_and_title() {
        let mut p = OscParser::new();
        assert_eq!(one(&mut p, b"\x1b]9;4;1;45\x07"), vec![OscEvent::Progress { operation: "set".into(), percentage: Some(45) }]);
        assert_eq!(one(&mut p, b"\x1b]0;proj \xe2\x80\x94 Claude\x07"), vec![OscEvent::Title { title: "proj — Claude".into() }]);
    }

    #[test]
    fn standalone_bell_and_csi_not_misparsed() {
        let mut p = OscParser::new();
        // a color CSI then a bell
        let ev = one(&mut p, b"\x1b[32mhi\x1b[0m\x07");
        assert_eq!(ev, vec![OscEvent::Bell]);
    }

    #[test]
    fn split_across_chunks() {
        let mut p = OscParser::new();
        assert!(p.feed(b"\x1b]21337;stat").is_empty());
        assert!(p.feed(b"us=Wait").is_empty());
        let ev = p.feed(b"ing\x07");
        assert_eq!(ev, vec![OscEvent::TabStatus { status: Some("Waiting".into()), indicator: None }]);
    }

    #[test]
    fn tmux_dcs_passthrough_unwrapped() {
        // inner = ESC ] 21337 ; status=Idle BEL ; ESC-doubled ; wrapped in ESC P tmux; … ST
        let mut wrapped = Vec::new();
        wrapped.extend_from_slice(b"\x1bPtmux;");
        wrapped.extend_from_slice(b"\x1b\x1b]21337;status=Idle\x07"); // doubled ESC
        wrapped.extend_from_slice(b"\x1b\\"); // ST
        let mut p = OscParser::new();
        let ev = p.feed(&wrapped);
        assert_eq!(ev, vec![OscEvent::TabStatus { status: Some("Idle".into()), indicator: None }]);
    }

    #[test]
    fn unknown_osc_ignored() {
        let mut p = OscParser::new();
        assert!(p.feed(b"\x1b]52;c;Zm9v\x07").is_empty()); // OSC 52 clipboard, ignored
    }
}
