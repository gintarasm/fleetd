//! Mouse and wheel encoding for the PTY (SGR, UTF-8 and X10 reports).

use fleet_term::Modes;

use super::keys::KeyMods;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Drag,
}

/// Encode a mouse event at 0-based `col`/`row`. `None` when the active modes
/// do not report this kind of event (or X10 cannot address the cell).
pub fn encode_mouse(
    button: MouseButton,
    kind: MouseKind,
    col: u16,
    row: u16,
    mods: KeyMods,
    modes: &Modes,
) -> Option<Vec<u8>> {
    let wheel = matches!(button, MouseButton::WheelUp | MouseButton::WheelDown);
    let reported = match kind {
        MouseKind::Press => modes.mouse_click || modes.mouse_drag || modes.mouse_motion,
        MouseKind::Release => {
            !wheel && (modes.mouse_click || modes.mouse_drag || modes.mouse_motion)
        }
        MouseKind::Drag => !wheel && (modes.mouse_drag || modes.mouse_motion),
    };
    if !reported {
        return None;
    }
    let base: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    let mod_bits = 4 * u32::from(mods.shift) + 8 * u32::from(mods.alt) + 16 * u32::from(mods.ctrl);
    let drag = if kind == MouseKind::Drag { 32 } else { 0 };
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);

    if modes.sgr_mouse {
        let code = base + mod_bits + drag;
        let end = if kind == MouseKind::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }

    // Legacy encodings report release as button 3 without identifying it.
    let b = if kind == MouseKind::Release { 3 } else { base };
    let code = b + mod_bits + drag;
    let mut out = b"\x1b[M".to_vec();
    if modes.utf8_mouse {
        for v in [code, x, y] {
            let ch = char::from_u32(32 + v).filter(|_| v + 32 <= 2047)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        for v in [code, x, y] {
            out.push(u8::try_from(32 + v).ok().filter(|_| v <= 223)?);
        }
    }
    Some(out)
}

/// Wheel scrolling on the alt screen turned into arrow keys (xterm's
/// alternate-scroll), when no mouse mode is active. Positive `lines` scrolls up.
pub fn wheel_as_arrows(lines: i32, modes: &Modes) -> Option<Vec<u8>> {
    let mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    if lines == 0 || !modes.alt_screen || !modes.alternate_scroll || mouse {
        return None;
    }
    let f = if lines > 0 { 'A' } else { 'B' };
    let one = if modes.app_cursor {
        format!("\x1bO{f}")
    } else {
        format!("\x1b[{f}")
    };
    Some(one.repeat(lines.unsigned_abs() as usize).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MODS: KeyMods = KeyMods {
        shift: false,
        alt: false,
        ctrl: false,
    };

    fn sgr(click: bool, drag: bool) -> Modes {
        Modes {
            mouse_click: click,
            mouse_drag: drag,
            sgr_mouse: true,
            ..Modes::default()
        }
    }

    fn enc(b: MouseButton, k: MouseKind, c: u16, r: u16, m: &Modes) -> Option<Vec<u8>> {
        encode_mouse(b, k, c, r, NO_MODS, m)
    }

    #[test]
    fn sgr_press_release_drag_wheel() {
        let m = sgr(true, true);
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Press, 0, 0, &m).unwrap(),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Release, 0, 0, &m).unwrap(),
            b"\x1b[<0;1;1m"
        );
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Drag, 4, 2, &m).unwrap(),
            b"\x1b[<32;5;3M"
        );
        assert_eq!(
            enc(MouseButton::WheelUp, MouseKind::Press, 1, 1, &m).unwrap(),
            b"\x1b[<64;2;2M"
        );
        assert_eq!(
            enc(MouseButton::WheelDown, MouseKind::Press, 1, 1, &m).unwrap(),
            b"\x1b[<65;2;2M"
        );
        let mods = KeyMods {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(
            encode_mouse(MouseButton::Right, MouseKind::Press, 0, 0, mods, &m).unwrap(),
            b"\x1b[<30;1;1M"
        );
    }

    #[test]
    fn unreported_kinds_are_none() {
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Drag, 0, 0, &sgr(true, false)),
            None
        );
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Press, 0, 0, &Modes::default()),
            None
        );
    }

    #[test]
    fn x10_and_utf8() {
        let x10 = Modes {
            mouse_click: true,
            ..Modes::default()
        };
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Press, 0, 0, &x10).unwrap(),
            b"\x1b[M !!"
        );
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Release, 0, 0, &x10).unwrap(),
            b"\x1b[M#!!"
        );
        assert_eq!(enc(MouseButton::Left, MouseKind::Press, 300, 0, &x10), None);
        assert_eq!(enc(MouseButton::Left, MouseKind::Press, 0, 223, &x10), None);
        assert!(enc(MouseButton::Left, MouseKind::Press, 222, 0, &x10).is_some());
        let utf8 = Modes {
            utf8_mouse: true,
            ..x10
        };
        assert_eq!(
            enc(MouseButton::Left, MouseKind::Press, 300, 0, &utf8).unwrap(),
            "\x1b[M \u{14d}!".as_bytes()
        );
    }

    #[test]
    fn wheel_as_arrows_only_on_alt_screen_with_alternate_scroll() {
        let on = Modes {
            alt_screen: true,
            alternate_scroll: true,
            ..Modes::default()
        };
        assert_eq!(wheel_as_arrows(2, &on).unwrap(), b"\x1b[A\x1b[A");
        assert_eq!(wheel_as_arrows(-1, &on).unwrap(), b"\x1b[B");
        let app = Modes {
            app_cursor: true,
            ..on
        };
        assert_eq!(wheel_as_arrows(1, &app).unwrap(), b"\x1bOA");
        assert_eq!(
            wheel_as_arrows(
                1,
                &Modes {
                    alt_screen: false,
                    ..on
                }
            ),
            None
        );
        assert_eq!(
            wheel_as_arrows(
                1,
                &Modes {
                    alternate_scroll: false,
                    ..on
                }
            ),
            None
        );
        assert_eq!(
            wheel_as_arrows(
                1,
                &Modes {
                    mouse_click: true,
                    ..on
                }
            ),
            None
        );
        assert_eq!(wheel_as_arrows(0, &on), None);
    }
}
