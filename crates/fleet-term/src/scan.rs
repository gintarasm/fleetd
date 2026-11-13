//! Finds `CSI 3 J` (erase saved lines) in PTY output, including sequences
//! split across [`Terminal::feed`](crate::Terminal::feed) calls.

/// Scanner state between bytes: how much of `ESC [ 3 J` has been seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// After `ESC [` and any leading zeros of the parameter.
    Csi,
    /// After `ESC [ 3`.
    Csi3,
}

/// Incremental `CSI 3 J` detector, approximating the VT parser: ESC starts
/// a new sequence anywhere, C0 controls inside a CSI are executed without
/// ending it (CAN and SUB abort it), and only a single parameter equal to 3
/// with no private marker or intermediate matches (`CSI 03 J` does).
#[derive(Debug, Default)]
pub(crate) struct ClearScan {
    state: State,
}

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

impl ClearScan {
    /// Scans `bytes` up to and including the first byte that completes a
    /// `CSI 3 J` and returns its index, or scans all of `bytes` and returns
    /// `None`. The scanner keeps its state, so the next call continues right
    /// after the returned index (or after `bytes`).
    pub(crate) fn find(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Ground {
                // Fast path: nothing to track until the next ESC.
                i += memchr::memchr(ESC, &bytes[i..])?;
            }
            let b = bytes[i];
            self.state = match (self.state, b) {
                (_, ESC) => State::Esc,
                (State::Esc, b'[') => State::Csi,
                (State::Csi, b'0') => State::Csi,
                (State::Csi, b'3') => State::Csi3,
                (State::Csi3, b'J') => {
                    self.state = State::Ground;
                    return Some(i);
                }
                (State::Csi | State::Csi3, CAN | SUB) => State::Ground,
                (state @ (State::Csi | State::Csi3), 0x00..=0x1f | 0x7f) => state,
                _ => State::Ground,
            };
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<(usize, usize)> {
        let mut scan = ClearScan::default();
        let mut found = Vec::new();
        for (c, chunk) in chunks.iter().enumerate() {
            let mut at = 0;
            while let Some(i) = scan.find(&chunk[at..]) {
                found.push((c, at + i));
                at += i + 1;
            }
        }
        found
    }

    #[test]
    fn finds_clear_in_one_chunk() {
        assert_eq!(all(&[b"ab\x1b[3Jcd"]), [(0, 5)]);
        assert_eq!(all(&[b"\x1b[3J\x1b[3J"]), [(0, 3), (0, 7)]);
    }

    #[test]
    fn finds_clear_split_across_chunks() {
        assert_eq!(all(&[b"x\x1b", b"[", b"3", b"Jy"]), [(3, 0)]);
        assert_eq!(all(&[b"\x1b[3", b"J"]), [(1, 0)]);
    }

    #[test]
    fn parameter_must_be_exactly_three() {
        for seq in [
            &b"\x1b[J"[..],
            b"\x1b[2J",
            b"\x1b[13J",
            b"\x1b[33J",
            b"\x1b[3;1J",
            b"\x1b[?3J",
            b"\x1b[3K",
            b"\x1b3J",
            b"[3J",
        ] {
            assert_eq!(all(&[seq]), [], "{seq:?}");
        }
        // Leading zeros denote the same parameter.
        assert_eq!(all(&[b"\x1b[03J"]), [(0, 4)]);
    }

    #[test]
    fn esc_restarts_and_controls_are_ignored_inside_csi() {
        assert_eq!(all(&[b"\x1b[\x1b[3J"]), [(0, 5)]);
        assert_eq!(all(&[b"\x1b[3\x1b[3J"]), [(0, 6)]);
        assert_eq!(all(&[b"\x1b[3\nJ"]), [(0, 4)]);
        // CAN aborts the sequence.
        assert_eq!(all(&[b"\x1b[3\x18J"]), []);
    }
}
