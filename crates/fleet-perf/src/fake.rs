//! Deterministic synthetic agent output for benchmarks.

/// Deterministic generator of agent-like terminal output.
///
/// Non-ANSI output is lines of exactly `line_len` printable ASCII bytes plus
/// `\n`. ANSI output adds SGR color codes and occasional `\x1b[2K\r` line
/// rewrites, as agent TUIs do.
pub struct FakeOutput {
    state: u64,
    line_len: usize,
    ansi: bool,
    pending: Vec<u8>,
    pos: usize,
}

impl FakeOutput {
    pub fn new(seed: u64, line_len: usize, ansi: bool) -> FakeOutput {
        // xorshift must not start at zero; mix the seed so small seeds differ well.
        let state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        FakeOutput {
            state: if state == 0 { 1 } else { state },
            line_len,
            ansi,
            pending: Vec::new(),
            pos: 0,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Visible text of exactly `line_len` bytes: lowercase words and spaces.
    fn words(&mut self) -> Vec<Vec<u8>> {
        let mut words = Vec::new();
        let mut used = 0;
        while used < self.line_len {
            let want = 2 + self.below(9) as usize;
            let len = want.min(self.line_len - used);
            let word: Vec<u8> = (0..len).map(|_| b'a' + self.below(26) as u8).collect();
            used += len;
            words.push(word);
            if used < self.line_len {
                used += 1; // separating space, appended on emit
                words.last_mut().unwrap().push(b' ');
            }
        }
        words
    }

    fn gen_line(&mut self) {
        self.pending.clear();
        self.pos = 0;
        if self.ansi && self.below(8) == 0 {
            self.pending.extend_from_slice(b"\x1b[2K\r");
        }
        let words = self.words();
        for word in words {
            if self.ansi && self.below(4) == 0 {
                let color = 31 + self.below(7);
                self.pending
                    .extend_from_slice(format!("\x1b[{color}m").as_bytes());
                self.pending.extend_from_slice(&word);
                self.pending.extend_from_slice(b"\x1b[0m");
            } else {
                self.pending.extend_from_slice(&word);
            }
        }
        self.pending.push(b'\n');
    }

    /// Fill `buf` completely with the next bytes of the stream.
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut written = 0;
        while written < buf.len() {
            if self.pos >= self.pending.len() {
                self.gen_line();
            }
            let n = (buf.len() - written).min(self.pending.len() - self.pos);
            buf[written..written + n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
            self.pos += n;
            written += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_vec(seed: u64, line_len: usize, ansi: bool, n: usize) -> Vec<u8> {
        let mut out = FakeOutput::new(seed, line_len, ansi);
        let mut buf = vec![0u8; n];
        out.fill(&mut buf);
        buf
    }

    #[test]
    fn same_seed_same_bytes() {
        assert_eq!(
            fill_vec(7, 80, true, 64 * 1024),
            fill_vec(7, 80, true, 64 * 1024)
        );
    }

    #[test]
    fn lines_have_requested_length() {
        let buf = fill_vec(1, 80, false, 64 * 1024);
        let lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
        assert!(lines.len() > 10);
        for line in &lines[..lines.len() - 1] {
            assert_eq!(line.len(), 80);
        }
    }

    #[test]
    fn ansi_mode_emits_escapes() {
        let buf = fill_vec(1, 80, true, 64 * 1024);
        assert!(buf.windows(2).any(|w| w == b"\x1b["));
    }
}
