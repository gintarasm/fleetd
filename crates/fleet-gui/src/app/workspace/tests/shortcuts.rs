use super::*;

// Issue #46: reopen closed pane, shortcut cheat sheet, first-run welcome.

// gpui keeps the debug bounds of earlier frames: what went away is
// checked on the state, not with `drawn`.
fn shortcuts_shown(s: &mut Setup<'_>) -> bool {
    s.cx.run_until_parked();
    s.ws.read_with(s.cx, |ws, _| ws.shortcuts_shown())
}

fn welcomes(s: &mut Setup<'_>) -> bool {
    s.cx.run_until_parked();
    s.app.read_with(s.cx, |a, cx| {
        a.layout().panes().iter().any(|p| {
            let chrome = &a.pane_view(*p).unwrap().read(cx).chrome;
            crate::app::pane_view::empty_state(chrome) == EmptyPane::Welcome
        })
    })
}

#[gpui::test]
fn cmd_shift_t_reopens_the_last_closed_pane_with_its_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    listed(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
    click(&mut s, "tab-1");
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-2");
    s.cx.simulate_keystrokes("cmd-w");
    assert_eq!(panes(&mut s), [PaneId(1)]);
    s.cx.simulate_keystrokes("cmd-shift-t");
    assert_eq!(panes(&mut s).len(), 2);
    let new = focused(&mut s);
    assert_eq!(shown(&mut s, new), Some(2));
    // Nothing more to reopen.
    s.cx.simulate_keystrokes("cmd-shift-t");
    assert_eq!(panes(&mut s).len(), 2);
    // Closing the last pane leaves it empty; ⌘⇧T fills it again.
    s.cx.simulate_keystrokes("cmd-w cmd-w");
    assert_eq!(panes(&mut s).len(), 1);
    let only = focused(&mut s);
    assert_eq!(shown(&mut s, only), None);
    s.cx.simulate_keystrokes("cmd-shift-t");
    assert_eq!(panes(&mut s).len(), 1);
    assert_eq!(shown(&mut s, only), Some(1));
}

#[gpui::test]
fn cmd_shift_t_skips_agents_that_are_gone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    listed(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
    click(&mut s, "tab-1");
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-2");
    s.cx.simulate_keystrokes("cmd-w");
    // Agent 2 was removed meanwhile.
    listed(&mut s, &fake, vec![row(1, "/a", 1)]);
    s.cx.simulate_keystrokes("cmd-shift-t");
    assert_eq!(panes(&mut s), [PaneId(1)]);
}

#[gpui::test]
fn cmd_slash_shows_every_key_binding_and_escape_closes_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    assert!(!shortcuts_shown(&mut s));
    s.cx.simulate_keystrokes("cmd-/");
    assert!(drawn(&mut s, "shortcuts-sheet"));
    // Generated from the keymap: every binding's keys are on it.
    let (keys, sheet) = s.cx.update(|_, cx| {
        let keys: Vec<String> = cx
            .key_bindings()
            .borrow()
            .bindings()
            .map(|b| {
                let strokes: Vec<String> = b.keystrokes().iter().map(|k| k.unparse()).collect();
                key_glyphs(&strokes.join(" "))
            })
            .collect();
        (keys, crate::app::keymap::cheat_sheet(cx))
    });
    assert!(keys.len() > 20, "{keys:?}");
    for k in &keys {
        assert!(
            sheet
                .iter()
                .any(|sec| sec.rows.iter().any(|r| &r.keys == k)),
            "{k} is missing from {sheet:?}"
        );
    }
    let window = &sheet[0];
    assert!(
        window
            .rows
            .iter()
            .any(|r| r.keys == "⇧⌘T" && r.title == "Reopen Closed Pane")
    );
    assert!(
        window
            .rows
            .iter()
            .any(|r| r.keys == "⌘/" && r.title == "Show Shortcuts")
    );
    // Panes ignore the keys meanwhile.
    s.cx.simulate_keystrokes("cmd-d");
    assert_eq!(panes(&mut s).len(), 1);
    s.cx.simulate_keystrokes("escape");
    assert!(!shortcuts_shown(&mut s));
    s.cx.simulate_keystrokes("cmd-/");
    assert!(shortcuts_shown(&mut s));
    s.cx.simulate_keystrokes("cmd-/");
    assert!(!shortcuts_shown(&mut s));
    // The keyboard is back in the pane.
    s.cx.simulate_keystrokes("cmd-d");
    assert_eq!(panes(&mut s).len(), 2);
}

#[gpui::test]
fn first_run_welcome_offers_new_agent_terminal_and_shortcuts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    // Before the list arrives: no welcome yet.
    assert!(!welcomes(&mut s));
    listed(&mut s, &fake, vec![]);
    assert!(welcomes(&mut s));
    assert!(drawn(&mut s, "welcome"));
    assert!(drawn(&mut s, "welcome-install-skill"));
    click(&mut s, "welcome-shortcuts");
    assert!(shortcuts_shown(&mut s));
    s.cx.simulate_keystrokes("escape");
    assert!(!shortcuts_shown(&mut s));
    click(&mut s, "welcome-new-agent");
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_none()));
    click(&mut s, "welcome-terminal");
    assert!(
        fake.calls().iter().any(|c| matches!(c, Call::Spawn { .. })),
        "{:?}",
        fake.calls()
    );
    // With an agent, an empty pane shows the hint only.
    listed(&mut s, &fake, vec![row(1, "/a", 1)]);
    assert!(!welcomes(&mut s));
}
