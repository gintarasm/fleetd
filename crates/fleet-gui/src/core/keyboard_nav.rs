//! Keyboard navigation in sheets and the Settings window (ADR 0101):
//! which key moves the focus, steps a control, or picks the next choice.
//! The views map gpui keystrokes onto these.

/// Where Tab moves the focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusMove {
    Next,
    Prev,
}

/// Tab is Next, Shift-Tab is Prev; with ⌘, ⌃ or ⌥ held it is something
/// else (a shortcut), so `None`. `other_modifiers`: any of those held.
pub fn focus_move(key: &str, shift: bool, other_modifiers: bool) -> Option<FocusMove> {
    if key != "tab" || other_modifiers {
        return None;
    }
    Some(if shift {
        FocusMove::Prev
    } else {
        FocusMove::Next
    })
}

/// The kind of control an arrow key steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// A segmented control (pills): ← and → only.
    Row,
    /// A stepper: ← and ↓ step down, → and ↑ step up.
    Stepper,
}

/// −1 or +1 for an unmodified arrow key the control takes.
pub fn arrow_step(key: &str, any_modifier: bool, axis: Axis) -> Option<i32> {
    if any_modifier {
        return None;
    }
    match (key, axis) {
        ("left", _) | ("down", Axis::Stepper) => Some(-1),
        ("right", _) | ("up", Axis::Stepper) => Some(1),
        _ => None,
    }
}

/// The choice `step` places from `current` in `all`; `None` at either end
/// (the selection stops there, it does not wrap) or when `current` is not
/// one of them.
pub fn step_choice<T: Copy + PartialEq>(all: &[T], current: T, step: i32) -> Option<T> {
    let at = all.iter().position(|c| *c == current)?;
    let next = at.checked_add_signed(step as isize)?;
    all.get(next).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_moves_forward_and_shift_tab_back() {
        assert_eq!(focus_move("tab", false, false), Some(FocusMove::Next));
        assert_eq!(focus_move("tab", true, false), Some(FocusMove::Prev));
    }

    #[test]
    fn tab_with_command_control_or_option_is_not_focus() {
        assert_eq!(focus_move("tab", false, true), None);
        assert_eq!(focus_move("tab", true, true), None);
        assert_eq!(focus_move("space", false, false), None);
    }

    #[test]
    fn pills_take_left_and_right_only() {
        assert_eq!(arrow_step("left", false, Axis::Row), Some(-1));
        assert_eq!(arrow_step("right", false, Axis::Row), Some(1));
        assert_eq!(arrow_step("up", false, Axis::Row), None);
        assert_eq!(arrow_step("down", false, Axis::Row), None);
        assert_eq!(arrow_step("right", true, Axis::Row), None);
    }

    #[test]
    fn steppers_take_all_four_arrows() {
        assert_eq!(arrow_step("left", false, Axis::Stepper), Some(-1));
        assert_eq!(arrow_step("down", false, Axis::Stepper), Some(-1));
        assert_eq!(arrow_step("right", false, Axis::Stepper), Some(1));
        assert_eq!(arrow_step("up", false, Axis::Stepper), Some(1));
        assert_eq!(arrow_step("a", false, Axis::Stepper), None);
    }

    #[test]
    fn choices_step_without_wrapping() {
        let all = ['a', 'b', 'c'];
        assert_eq!(step_choice(&all, 'a', 1), Some('b'));
        assert_eq!(step_choice(&all, 'b', -1), Some('a'));
        assert_eq!(step_choice(&all, 'c', 1), None);
        assert_eq!(step_choice(&all, 'a', -1), None);
        assert_eq!(step_choice(&all, 'z', 1), None);
    }
}
