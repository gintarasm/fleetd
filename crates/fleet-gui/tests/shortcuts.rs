//! Issue #46: the shortcut cheat sheet (generated from the key bindings),
//! the recently closed panes ⌘⇧T reopens, and the first-run welcome.

use fleet_gui::core::shortcuts::{
    Binding, ClosedPanes, EmptyPane, Row, cheat_sheet, empty_pane, key_glyphs,
};

fn b<'a>(keys: &'a str, action: &'a str, context: Option<&'a str>) -> Binding<'a> {
    Binding {
        keys,
        action,
        context,
    }
}

fn row(keys: &str, title: &str) -> Row {
    Row {
        keys: keys.to_owned(),
        title: title.to_owned(),
    }
}

#[test]
fn keys_are_drawn_as_mac_glyphs_in_menu_order() {
    assert_eq!(key_glyphs("cmd-d"), "⌘D");
    assert_eq!(key_glyphs("cmd-shift-d"), "⇧⌘D");
    assert_eq!(key_glyphs("ctrl-alt-shift-cmd-x"), "⌃⌥⇧⌘X");
    assert_eq!(key_glyphs("cmd-shift-enter"), "⇧⌘↩");
    assert_eq!(key_glyphs("cmd-alt-left"), "⌥⌘←");
    assert_eq!(key_glyphs("ctrl-tab"), "⌃⇥");
    assert_eq!(key_glyphs("cmd-/"), "⌘/");
    assert_eq!(key_glyphs("cmd-,"), "⌘,");
    assert_eq!(key_glyphs("cmd-="), "⌘=");
    // The minus key itself.
    assert_eq!(key_glyphs("cmd--"), "⌘-");
    assert_eq!(key_glyphs("escape"), "⎋");
    assert_eq!(key_glyphs("f5"), "F5");
    // A chord: its keystrokes in turn.
    assert_eq!(key_glyphs("cmd-k cmd-s"), "⌘K ⌘S");
}

#[test]
fn the_sheet_lists_every_binding_by_context_with_a_title() {
    let sheet = cheat_sheet([
        b("cmd-d", "fleet::SplitRight", None),
        b("cmd-c", "fleet::Copy", Some("Terminal")),
        b("cmd-r", "fleet::RestartPane", Some("Workspace")),
        b("cmd-z", "fleet::Undo", Some("TextField")),
        b("cmd-shift-t", "fleet::ReopenClosedPane", None),
    ]);
    let titles: Vec<_> = sheet.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Window", "Terminal", "Text Fields"]);
    assert_eq!(
        sheet[0].rows,
        [
            row("⌘D", "Split Right"),
            row("⌘R", "Restart Pane"),
            row("⇧⌘T", "Reopen Closed Pane"),
        ]
    );
    assert_eq!(sheet[1].rows, [row("⌘C", "Copy")]);
    assert_eq!(sheet[2].rows, [row("⌘Z", "Undo")]);
}

#[test]
fn an_action_the_sheet_never_heard_of_still_shows() {
    // Bindings added later (⌘P palette, ⌘F find, …) need no list here.
    let sheet = cheat_sheet([
        b("cmd-p", "fleet::CommandPalette", None),
        b("cmd-1", "fleet::FocusPane1", None),
        b("cmd-=", "zoom::ZoomIn", None),
        b("cmd-f", "Find", Some("Search")),
    ]);
    assert_eq!(
        sheet[0].rows,
        [
            row("⌘P", "Command Palette"),
            row("⌘1", "Focus Pane 1"),
            row("⌘=", "Zoom In"),
        ]
    );
    assert_eq!(sheet[1].title, "Search");
    assert_eq!(sheet[1].rows, [row("⌘F", "Find")]);
}

#[test]
fn a_later_binding_of_the_same_keys_wins_and_duplicates_show_once() {
    // As in gpui, the binding added last takes the keystroke.
    let sheet = cheat_sheet([
        b("cmd-v", "fleet::Paste", Some("Terminal")),
        b("cmd-k", "fleet::Old", None),
        b("cmd-v", "fleet::Paste", Some("Terminal")),
        b("cmd-k", "fleet::New", None),
    ]);
    assert_eq!(sheet[0].rows, [row("⌘K", "New")]);
    assert_eq!(sheet[1].rows, [row("⌘V", "Paste")]);
}

#[test]
fn closed_panes_reopen_newest_first_skipping_forgotten_agents() {
    let mut closed = ClosedPanes::default();
    closed.push(1);
    closed.push(2);
    closed.push(3);
    // Agent 3 was removed meanwhile: it is dropped, 2 reopens.
    assert_eq!(closed.pop(|id| id != 3), Some(2));
    assert_eq!(closed.pop(|_| true), Some(1));
    assert_eq!(closed.pop(|_| true), None);
}

#[test]
fn closed_panes_keep_a_small_stack_without_repeats() {
    let mut closed = ClosedPanes::default();
    for id in 0..(ClosedPanes::CAP as u64 + 5) {
        closed.push(id);
    }
    assert_eq!(closed.len(), ClosedPanes::CAP);
    // Closing the same agent again moves it to the top, once.
    closed.push(7);
    closed.push(7);
    assert_eq!(closed.len(), ClosedPanes::CAP);
    assert_eq!(closed.pop(|_| true), Some(7));
    assert_eq!(closed.pop(|_| true), Some(ClosedPanes::CAP as u64 + 4));
    closed.clear();
    assert_eq!(closed.pop(|_| true), None);
}

#[test]
fn an_empty_pane_welcomes_only_while_there_are_no_agents() {
    // Not yet listed: the plain hint, so the welcome does not flash.
    assert_eq!(empty_pane(false, false, false), EmptyPane::Hint);
    assert_eq!(empty_pane(false, false, true), EmptyPane::Welcome);
    assert_eq!(empty_pane(false, true, true), EmptyPane::Starting);
    assert_eq!(empty_pane(true, false, true), EmptyPane::None);
    assert_eq!(empty_pane(true, true, false), EmptyPane::None);
}
