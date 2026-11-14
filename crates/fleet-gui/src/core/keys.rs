//! Key and paste encoding for the PTY: legacy xterm sequences plus the kitty
//! keyboard protocol's "disambiguate" level. Pure functions, no gpui types.

use fleet_term::Modes;

/// Kitty flag bit for "disambiguate escape codes".
const KITTY_DISAMBIGUATE: u8 = 0b1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    /// The base character of the key (as gpui's `key`).
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key, 1..=12.
    F(u8),
}

/// Cmd is never encoded; the shell treats cmd-keys as app shortcuts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl KeyMods {
    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }

    /// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    pub(crate) fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyMods,
    /// The layout's composed text (gpui `key_char`).
    pub text: Option<String>,
}

/// Encode a key press. `None` means "let the IME/input handler insert the text".
pub fn encode_key(key: &Key, modes: &Modes, option_as_meta: bool) -> Option<Vec<u8>> {
    let kitty = modes.kitty_flags & KITTY_DISAMBIGUATE != 0;
    let m = key.mods;
    match key.code {
        KeyCode::Char(c) => encode_char(key, c, kitty, option_as_meta),
        KeyCode::Enter => Some(if kitty && m.any() {
            csi_u(13, m)
        } else {
            with_alt(
                KeyMods {
                    alt: m.alt || m.shift,
                    ..m
                },
                b"\r".to_vec(),
            )
        }),
        KeyCode::Tab => Some(if m.shift && !m.alt && !m.ctrl {
            b"\x1b[Z".to_vec()
        } else {
            with_alt(m, b"\t".to_vec())
        }),
        KeyCode::Backspace => Some(with_alt(m, b"\x7f".to_vec())),
        KeyCode::Escape => Some(if kitty {
            csi_u(27, m)
        } else {
            with_alt(m, b"\x1b".to_vec())
        }),
        KeyCode::Up => Some(cursor_key(b'A', m, modes.app_cursor)),
        KeyCode::Down => Some(cursor_key(b'B', m, modes.app_cursor)),
        KeyCode::Right => Some(cursor_key(b'C', m, modes.app_cursor)),
        KeyCode::Left => Some(cursor_key(b'D', m, modes.app_cursor)),
        KeyCode::Home => Some(cursor_key(b'H', m, modes.app_cursor)),
        KeyCode::End => Some(cursor_key(b'F', m, modes.app_cursor)),
        KeyCode::PageUp => Some(tilde_key(5, m)),
        KeyCode::PageDown => Some(tilde_key(6, m)),
        KeyCode::Insert => Some(tilde_key(2, m)),
        KeyCode::Delete => Some(tilde_key(3, m)),
        KeyCode::F(n) => function_key(n, m),
    }
}

fn encode_char(key: &Key, c: char, kitty: bool, option_as_meta: bool) -> Option<Vec<u8>> {
    let m = key.mods;
    if m.alt && !option_as_meta {
        // Option composes text (e.g. alt-s -> ß); the IME inserts it.
        return None;
    }
    if !m.ctrl && !m.alt {
        return None;
    }
    if kitty {
        let base = c.to_lowercase().next().unwrap_or(c);
        return Some(csi_u(base as u32, m));
    }
    if m.ctrl {
        let byte = ctrl_byte(c)?;
        return Some(with_alt(m, vec![byte]));
    }
    // Option-as-Meta: ESC followed by the (shifted, when shift is held) character.
    let ch = if m.shift {
        key.text
            .as_deref()
            .and_then(|t| t.chars().next())
            .unwrap_or_else(|| c.to_uppercase().next().unwrap_or(c))
    } else {
        c
    };
    let mut out = vec![0x1b];
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(out)
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '8' => Some(0x7f),
        _ => None,
    }
}

fn with_alt(m: KeyMods, mut bytes: Vec<u8>) -> Vec<u8> {
    if m.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// `CSI codepoint ; mods u`, omitting the modifier when there are none.
fn csi_u(code: u32, m: KeyMods) -> Vec<u8> {
    if m.any() {
        format!("\x1b[{code};{}u", m.param()).into_bytes()
    } else {
        format!("\x1b[{code}u").into_bytes()
    }
}

fn cursor_key(final_byte: u8, m: KeyMods, app_cursor: bool) -> Vec<u8> {
    let f = final_byte as char;
    if m.any() {
        format!("\x1b[1;{}{f}", m.param()).into_bytes()
    } else if app_cursor {
        format!("\x1bO{f}").into_bytes()
    } else {
        format!("\x1b[{f}").into_bytes()
    }
}

fn tilde_key(n: u8, m: KeyMods) -> Vec<u8> {
    if m.any() {
        format!("\x1b[{n};{}~", m.param()).into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn function_key(n: u8, m: KeyMods) -> Option<Vec<u8>> {
    match n {
        1..=4 => {
            let f = char::from(b"PQRS"[usize::from(n - 1)]);
            Some(if m.any() {
                format!("\x1b[1;{}{f}", m.param()).into_bytes()
            } else {
                format!("\x1bO{f}").into_bytes()
            })
        }
        5..=12 => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            Some(tilde_key(CODES[usize::from(n - 5)], m))
        }
        _ => None,
    }
}

/// Encode pasted text. Newlines become `\r`; with bracketed paste the text is
/// wrapped in `\x1b[200~`/`\x1b[201~` and any embedded end marker is removed.
pub fn encode_paste(text: &str, modes: &Modes) -> Vec<u8> {
    const END: &str = "\x1b[201~";
    let mut body = if modes.bracketed_paste {
        // Remove until stable so a marker cannot be rebuilt from the remains.
        let mut s = text.to_string();
        while s.contains(END) {
            s = s.replace(END, "");
        }
        s
    } else {
        text.to_string()
    };
    body = body.replace("\r\n", "\r").replace('\n', "\r");
    if modes.bracketed_paste {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(END.as_bytes());
        out
    } else {
        body.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, shift: bool, alt: bool, ctrl: bool, text: Option<&str>) -> Key {
        Key {
            code,
            mods: KeyMods { shift, alt, ctrl },
            text: text.map(str::to_string),
        }
    }

    fn plain(code: KeyCode) -> Key {
        key(code, false, false, false, None)
    }

    fn kitty() -> Modes {
        Modes {
            kitty_flags: 1,
            ..Modes::default()
        }
    }

    #[test]
    fn key_table() {
        let none = Modes::default();
        let app = Modes {
            app_cursor: true,
            ..Modes::default()
        };
        let c = KeyCode::Char;
        let rows: Vec<(Key, Modes, bool, Option<&[u8]>)> = vec![
            (
                key(c('a'), false, false, false, Some("a")),
                none,
                true,
                None,
            ),
            (
                key(c('c'), false, false, true, None),
                none,
                true,
                Some(&[0x03]),
            ),
            (
                key(c('['), false, false, true, None),
                none,
                true,
                Some(&[0x1b]),
            ),
            (
                key(c(' '), false, false, true, None),
                none,
                true,
                Some(&[0x00]),
            ),
            (
                key(c('b'), false, true, false, Some("∫")),
                none,
                true,
                Some(b"\x1bb"),
            ),
            (
                key(c('b'), true, true, false, Some("B")),
                none,
                true,
                Some(b"\x1bB"),
            ),
            (
                key(c('s'), false, true, false, Some("ß")),
                none,
                false,
                None,
            ),
            (plain(KeyCode::Enter), none, true, Some(b"\r")),
            (plain(KeyCode::Tab), none, true, Some(b"\t")),
            (
                key(KeyCode::Tab, true, false, false, None),
                none,
                true,
                Some(b"\x1b[Z"),
            ),
            (plain(KeyCode::Backspace), none, true, Some(b"\x7f")),
            (
                key(KeyCode::Backspace, false, true, false, None),
                none,
                true,
                Some(b"\x1b\x7f"),
            ),
            (plain(KeyCode::Escape), none, true, Some(b"\x1b")),
            (plain(KeyCode::Up), none, true, Some(b"\x1b[A")),
            (plain(KeyCode::Up), app, true, Some(b"\x1bOA")),
            (
                key(KeyCode::Up, true, false, false, None),
                none,
                true,
                Some(b"\x1b[1;2A"),
            ),
            (
                key(KeyCode::Left, false, true, false, None),
                none,
                true,
                Some(b"\x1b[1;3D"),
            ),
            (plain(KeyCode::Home), none, true, Some(b"\x1b[H")),
            (plain(KeyCode::PageUp), none, true, Some(b"\x1b[5~")),
            (plain(KeyCode::Delete), none, true, Some(b"\x1b[3~")),
            (plain(KeyCode::F(1)), none, true, Some(b"\x1bOP")),
            (plain(KeyCode::F(5)), none, true, Some(b"\x1b[15~")),
            (plain(KeyCode::F(12)), none, true, Some(b"\x1b[24~")),
            (
                key(KeyCode::Enter, true, false, false, None),
                none,
                true,
                Some(b"\x1b\r"),
            ),
            (
                key(KeyCode::Enter, true, false, false, None),
                kitty(),
                true,
                Some(b"\x1b[13;2u"),
            ),
            (plain(KeyCode::Escape), kitty(), true, Some(b"\x1b[27u")),
            (
                key(c('c'), false, false, true, None),
                kitty(),
                true,
                Some(b"\x1b[99;5u"),
            ),
            (
                key(c('a'), false, true, false, Some("å")),
                kitty(),
                true,
                Some(b"\x1b[97;3u"),
            ),
            (plain(KeyCode::Enter), kitty(), true, Some(b"\r")),
            (plain(KeyCode::Tab), kitty(), true, Some(b"\t")),
            (plain(KeyCode::Backspace), kitty(), true, Some(b"\x7f")),
            (
                key(c('a'), false, false, false, Some("a")),
                kitty(),
                true,
                None,
            ),
        ];
        for (i, (k, modes, meta, want)) in rows.into_iter().enumerate() {
            assert_eq!(
                encode_key(&k, &modes, meta).as_deref(),
                want,
                "row {i}: {k:?}"
            );
        }
    }

    #[test]
    fn paste_brackets_and_sanitises() {
        let on = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_paste("a\n\x1b[201~b", &on),
            b"\x1b[200~a\rb\x1b[201~".to_vec()
        );
        assert_eq!(
            encode_paste("a\n\x1b[201~b", &Modes::default()),
            b"a\r\x1b[201~b".to_vec()
        );
    }
}
