//! Channel 1 encoding — UTF-8-safe tail buffering (ARCHITECTURE §3.1, fix B3).
//!
//! PTY output is binary; a multibyte UTF-8 sequence can be split across read
//! boundaries (8 KiB chunks). `from_utf8_lossy` would corrupt such splits into
//! U+FFFD. Instead we emit only complete valid UTF-8 and hold back an incomplete
//! trailing sequence until the next chunk. ANSI control bytes are all ASCII, so
//! they are never split.

/// Accumulates raw bytes and yields the largest complete-UTF-8 prefix available.
#[derive(Default)]
pub struct Utf8TailBuffer {
    pending: Vec<u8>,
}

impl Utf8TailBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed new bytes; return the decodable UTF-8 now available (possibly empty).
    /// An incomplete trailing multibyte sequence is retained for the next call.
    /// A genuinely invalid byte sequence is replaced with U+FFFD so we make progress.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // SAFETY: `valid` is a validated UTF-8 boundary from `from_utf8`.
                        out.push_str(unsafe { std::str::from_utf8_unchecked(&self.pending[..valid]) });
                    }
                    match e.error_len() {
                        // Incomplete tail — keep it, wait for more bytes.
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                        // Invalid sequence of `bad` bytes — emit replacement, skip them, continue.
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
        out
    }

    /// Bytes currently held back (an incomplete trailing sequence).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_control_passthrough() {
        let mut b = Utf8TailBuffer::new();
        assert_eq!(b.push(b"\x1b[32mHello\x1b[0m\n"), "\x1b[32mHello\x1b[0m\n");
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn multibyte_split_across_pushes_is_lossless() {
        // "café" = 63 61 66 C3 A9 ; split before the final byte of 'é'.
        let mut b = Utf8TailBuffer::new();
        assert_eq!(b.push(&[0x63, 0x61, 0x66, 0xC3]), "caf");
        assert_eq!(b.pending_len(), 1);
        assert_eq!(b.push(&[0xA9]), "é");
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn emoji_split_three_ways() {
        // 😀 = F0 9F 98 80
        let mut b = Utf8TailBuffer::new();
        assert_eq!(b.push(&[0xF0, 0x9F]), "");
        assert_eq!(b.push(&[0x98]), "");
        assert_eq!(b.push(&[0x80]), "😀");
    }

    #[test]
    fn invalid_byte_becomes_replacement_and_makes_progress() {
        let mut b = Utf8TailBuffer::new();
        // 0xFF is never valid UTF-8.
        assert_eq!(b.push(&[0x41, 0xFF, 0x42]), "A\u{FFFD}B");
        assert_eq!(b.pending_len(), 0);
    }
}
