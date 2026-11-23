use super::*;

fn hover(s: &mut Setup<'_>, selector: &'static str) {
    let at = center(s, selector);
    s.cx.simulate_mouse_move(at, None, Modifiers::none());
    s.cx.run_until_parked();
}

/// A shell from New terminal is asked about as a shell, not an agent
/// (ADR 0071).
#[gpui::test]
fn kill_asks_about_a_shell_as_a_shell(cx: &mut TestAppContext) {
    use crate::core::spawn::{KIND_LABEL, SHELL_KIND};
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut shell = row(4, "/w", 1);
    shell
        .info
        .labels
        .insert(KIND_LABEL.to_owned(), SHELL_KIND.to_owned());
    agents(&mut s, &fake, vec![shell]);
    s.cx.dispatch_action(Kill { agent_id: 4 });
    assert_eq!(kill_asked(&mut s), Some(4));
    let question = s.ws.read_with(s.cx, |w, cx| {
        w.kill_sheet().unwrap().read(cx).question.to_string()
    });
    assert_eq!(question, "Kill shell 4 (sh, /w)?");
}

#[gpui::test]
fn kill_asks_in_a_sheet_and_only_kill_kills(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    assert_eq!(kill_asked(&mut s), Some(7));
    assert!(drawn(&mut s, "kill-sheet"));
    let question = s.ws.read_with(s.cx, |w, cx| {
        w.kill_sheet().unwrap().read(cx).question.to_string()
    });
    assert_eq!(question, "Kill agent 7 (sh, /w)?");
    // The sheet is modal: ⌘W does not close the pane meanwhile.
    s.cx.simulate_keystrokes("cmd-w");
    assert!(!fake.calls().contains(&Call::Close(PaneId(1))));
    click(&mut s, "kill-cancel");
    assert_eq!(kill_asked(&mut s), None);
    assert!(kills(&fake).is_empty());

    s.cx.dispatch_action(Kill { agent_id: 7 });
    click(&mut s, "kill-confirm");
    assert_eq!(kill_asked(&mut s), None);
    assert_eq!(kills(&fake), [7]);
    // Still asks next time; nothing was saved.
    assert!(s.app.read_with(s.cx, |a, _| a.config.confirm_kill));
    assert!(!dir.path().join("config.toml").exists());
}

#[gpui::test]
fn escape_cancels_the_kill_and_return_kills(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    assert_eq!(kill_asked(&mut s), Some(7));
    s.cx.simulate_keystrokes("escape");
    assert_eq!(kill_asked(&mut s), None);
    assert!(kills(&fake).is_empty());
    s.cx.dispatch_action(Kill { agent_id: 7 });
    s.cx.simulate_keystrokes("enter");
    assert_eq!(kill_asked(&mut s), None);
    assert_eq!(kills(&fake), [7]);
    // The keyboard is back in the pane.
    let app = s.app.clone();
    let back = s.cx.update(|window, cx| {
        let view = app.read(cx).pane_view(PaneId(1)).unwrap().clone();
        view.read(cx).focus_handle().is_focused(window)
    });
    assert!(back);
}

#[gpui::test]
fn dont_ask_again_is_saved_and_skips_the_question(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "# mine\n[gui]\nfont_size = 15\n",
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    click(&mut s, "kill-dont-ask");
    // Checking it kills nothing by itself; Cancel would still cancel.
    assert!(kills(&fake).is_empty());
    click(&mut s, "kill-confirm");
    assert_eq!(kills(&fake), [7]);
    let config = crate::core::config::load_config(dir.path()).unwrap();
    assert!(!config.confirm_kill);
    assert_eq!(config.font_size, 15.0);
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(text.contains("# mine"), "{text}");
    // From now on, no question.
    s.cx.dispatch_action(Kill { agent_id: 8 });
    assert_eq!(kill_asked(&mut s), None);
    assert_eq!(kills(&fake), [7, 8]);
}

/// A key pressed and released (gpui's keyboard click is on key up).
fn press(s: &mut Setup<'_>, key: &str) {
    s.cx.simulate_keystrokes(key);
    s.cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse(key).unwrap(),
    });
    s.cx.run_until_parked();
}

fn dont_ask(s: &mut Setup<'_>) -> bool {
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.kill_sheet().cloned())
            .unwrap();
    sheet.read_with(s.cx, |k, _| k.dont_ask)
}

#[gpui::test]
fn kill_sheet_tabs_to_its_controls_and_space_clicks_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    // As it opens, Space toggles the checkbox.
    press(&mut s, "space");
    assert!(dont_ask(&mut s));
    press(&mut s, "space");
    assert!(!dont_ask(&mut s));
    // Tab: the checkbox, which Space toggles once.
    press(&mut s, "tab");
    press(&mut s, "space");
    assert!(dont_ask(&mut s));
    // Tab: Cancel, which Space presses.
    press(&mut s, "tab");
    press(&mut s, "space");
    assert_eq!(kill_asked(&mut s), None, "cancelled");
    assert!(kills(&fake).is_empty());

    // Return is the default button (Kill) even on a focused Cancel.
    s.cx.dispatch_action(Kill { agent_id: 7 });
    press(&mut s, "tab");
    press(&mut s, "tab");
    press(&mut s, "enter");
    assert_eq!(kills(&fake), [7]);
    assert!(
        crate::core::config::load_config(dir.path())
            .unwrap()
            .confirm_kill
    );
}

#[gpui::test]
fn new_agent_sheet_tabs_through_its_fields(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-n");
    s.cx.run_until_parked();
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let focused = |s: &mut Setup<'_>, f: fn(&NewAgentSheet) -> &Entity<TextField>| {
        s.cx.update(|window, cx| f(sheet.read(cx)).read(cx).focus_handle().is_focused(window))
    };
    assert!(focused(&mut s, |sh| &sh.command));
    press(&mut s, "tab");
    assert!(focused(&mut s, |sh| &sh.prompt));
    s.cx.simulate_input("fix it");
    let prompt = sheet.read_with(s.cx, |sh, cx| sh.prompt.read(cx).text().to_owned());
    assert_eq!(prompt, "fix it");
    press(&mut s, "tab");
    assert!(focused(&mut s, |sh| &sh.name));
    press(&mut s, "tab");
    assert!(focused(&mut s, |sh| &sh.labels));
    // Back past Name, Prompt, Command and Choose… to the directory.
    for _ in 0..5 {
        press(&mut s, "shift-tab");
    }
    assert!(focused(&mut s, |sh| &sh.dir));
    // ⌘Tab is not focus movement.
    s.cx.simulate_keystrokes("cmd-tab");
    assert!(focused(&mut s, |sh| &sh.dir));
}

#[gpui::test]
fn confirm_kill_off_kills_at_once(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        confirm_kill: false,
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    assert_eq!(kill_asked(&mut s), None);
    assert_eq!(kills(&fake), [7]);
}

#[gpui::test]
fn cmd_shift_w_kills_the_focused_panes_running_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    // An empty pane: nothing to kill.
    s.cx.simulate_keystrokes("cmd-shift-w");
    assert_eq!(kill_asked(&mut s), None);
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    click(&mut s, "tab-7");
    s.cx.simulate_keystrokes("cmd-shift-w");
    assert_eq!(kill_asked(&mut s), Some(7));
    s.cx.simulate_keystrokes("escape");
    // Not for an agent that exited.
    agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
    s.cx.simulate_keystrokes("cmd-shift-w");
    assert_eq!(kill_asked(&mut s), None);
    assert!(kills(&fake).is_empty());
    // The pane stays open either way.
    assert_eq!(panes(&mut s), [PaneId(1)]);
}

#[gpui::test]
fn the_pane_header_offers_stop_or_restart_and_remove(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    assert!(!drawn(&mut s, "pane-1-stop"), "an empty pane has no Stop");
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    click(&mut s, "tab-7");
    attach_model(&fake, PaneId(1), 7);
    assert!(!drawn(&mut s, "pane-1-remove"));
    // Always shown on the focused pane.
    click(&mut s, "pane-1-stop");
    assert_eq!(kill_asked(&mut s), Some(7));
    click(&mut s, "kill-confirm");
    assert_eq!(kills(&fake), [7]);
    // Close (×) still only detaches.
    assert!(!fake.calls().contains(&Call::Close(PaneId(1))));

    // Ended: Restart and Remove instead. (gpui's test frames keep the
    // bounds of elements drawn before, so Stop's absence is checked in
    // `core::kill`.)
    agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    click(&mut s, "pane-1-restart-agent");
    assert!(
        fake.calls()
            .iter()
            .any(|c| matches!(c, Call::RestartInPane { agent_id: 7, .. })),
        "{:?}",
        fake.calls()
    );
    click(&mut s, "pane-1-remove");
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![7])));
}

#[gpui::test]
fn an_unfocused_panes_stop_shows_on_hover(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    click(&mut s, "tab-7");
    // Pane 2 takes the focus; pane 1 keeps agent 7.
    s.cx.simulate_keystrokes("cmd-d");
    assert_eq!(focused(&mut s), PaneId(2));
    // Hovering the header shows the button; a click on it asks.
    hover(&mut s, "pane-1-stop");
    click(&mut s, "pane-1-stop");
    assert_eq!(kill_asked(&mut s), Some(7));
}

#[gpui::test]
fn the_pane_menu_kills_its_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    click(&mut s, "tab-7");
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::Kill, epoch, window, cx);
    });
    assert_eq!(kill_asked(&mut s), Some(7));
}

#[gpui::test]
fn a_rows_buttons_act_on_hover_without_showing_the_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
    );
    assert!(!drawn(&mut s, "tab-7-restart") && !drawn(&mut s, "tab-8-stop"));
    hover(&mut s, "tab-7");
    click(&mut s, "tab-7-stop");
    assert_eq!(kill_asked(&mut s), Some(7));
    s.cx.simulate_keystrokes("escape");
    // The row's own click did not run.
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Attach(_, 7))));
    hover(&mut s, "tab-8");
    click(&mut s, "tab-8-restart");
    assert_eq!(fake.calls().last(), Some(&Call::Restart(8)));
    hover(&mut s, "tab-8");
    click(&mut s, "tab-8-remove");
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8])));
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Attach(_, 8))));
}

#[gpui::test]
fn kill_asks_nothing_when_settings_turn_confirmation_off(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        confirm_kill: false,
        ..GuiConfig::default()
    };
    let s = setup_with(cx, &fake, dir.path(), config);
    s.cx.dispatch_action(Kill { agent_id: 7 });
    s.cx.run_until_parked();
    assert!(!s.cx.has_pending_prompt());
    assert!(fake.calls().contains(&Call::Kill(7)));
}
