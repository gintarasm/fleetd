use super::*;

fn send_sheet(s: &mut Setup<'_>) -> Option<Entity<SendSheet>> {
    s.ws.read_with(s.cx, |w, _| w.send_sheet().cloned())
}

/// ⇧⌘M: the message, two agents checked from the filter (by label),
/// Send; the text goes now, the Return a moment later.
#[gpui::test]
fn send_to_agents_types_into_the_checked_agents(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            named(1, "alpha", &[("role", "reviewer")]),
            named(2, "beta", &[("role", "reviewer")]),
            named(3, "gamma", &[("role", "coder")]),
            ended(4, "/w", 4, AgentState::Exited),
        ],
    );
    s.cx.simulate_keystrokes("cmd-shift-m");
    let sheet = send_sheet(&mut s).expect("send sheet open");
    // Running agents only.
    let listed = sheet.read_with(s.cx, |sh, cx| sh.picker.read(cx).picker().items().len());
    assert_eq!(listed, 3);
    // Return in the message box is a line break, not Send.
    s.cx.simulate_input("rebase");
    s.cx.simulate_keystrokes("enter");
    s.cx.simulate_input("on main");
    let text = sheet.read_with(s.cx, |sh, cx| sh.message.read(cx).text().to_owned());
    assert_eq!(text, "rebase\non main");
    // Tab to the filter; Return checks the best match.
    s.cx.simulate_keystrokes("tab");
    s.cx.simulate_input("beta");
    s.cx.simulate_keystrokes("enter");
    sheet.update(s.cx, |sh, cx| sh.toggle(1, cx));
    let picked = sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec());
    assert_eq!(picked, [2, 1]);
    s.cx.simulate_keystrokes("cmd-enter");
    assert!(send_sheet(&mut s).is_none(), "sent and closed");
    // As typed (no pane asked for bracketed paste), the Return later.
    assert_eq!(
        sends(&fake),
        [
            (2, b"rebase\non main".to_vec()),
            (1, b"rebase\non main".to_vec())
        ]
    );
    s.cx.executor()
        .advance_clock(std::time::Duration::from_millis(RETURN_DELAY_MS));
    s.cx.run_until_parked();
    assert_eq!(
        sends(&fake)[2..],
        [(2, b"\r".to_vec()), (1, b"\r".to_vec())]
    );
}

/// A working target is warned about first; Send again sends. With
/// Return after off, only the text goes.
#[gpui::test]
fn send_warns_before_typing_into_a_working_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut busy = named(5, "busy", &[]);
    busy.last_output_unix_ms = Some(now_ms());
    agents(&mut s, &fake, vec![busy, named(6, "calm", &[])]);
    pick_row(&mut s, 5, RowMenuItem::SendTo);
    let sheet = send_sheet(&mut s).expect("send sheet open");
    assert_eq!(
        sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec()),
        [5]
    );
    s.cx.simulate_input("stop");
    click(&mut s, "send-return");
    click(&mut s, "send-confirm");
    let warning = sheet.read_with(s.cx, |sh, _| sh.warning.clone());
    assert_eq!(
        warning.as_deref(),
        Some("busy is working: the text lands mid-turn. Send anyway?")
    );
    assert!(sends(&fake).is_empty(), "nothing sent yet");
    click(&mut s, "send-confirm");
    assert!(send_sheet(&mut s).is_none());
    s.cx.executor()
        .advance_clock(std::time::Duration::from_millis(RETURN_DELAY_MS));
    s.cx.run_until_parked();
    assert_eq!(sends(&fake), [(5, b"stop".to_vec())]);
    // Escape cancels without sending.
    s.cx.simulate_keystrokes("cmd-shift-m");
    assert!(send_sheet(&mut s).is_some());
    s.cx.simulate_keystrokes("escape");
    assert!(send_sheet(&mut s).is_none());
    assert_eq!(sends(&fake).len(), 1);
}

/// A multi-line text goes to an agent whose pane asked for bracketed
/// paste as one paste.
#[gpui::test]
fn send_pastes_several_lines_where_the_pane_asks_for_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![named(3, "shown", &[])]);
    attach_model(&fake, PaneId(1), 3);
    fake.model(PaneId(1))
        .lock()
        .unwrap()
        .on_output(b"\x1b[?2004h");
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.app.update(s.cx, |a, cx| {
        a.send_text(
            &SendPlan {
                text: "a\nb".into(),
                targets: vec![3],
                press_return: false,
            },
            cx,
        );
    });
    assert_eq!(sends(&fake), [(3, b"\x1b[200~a\rb\x1b[201~".to_vec())]);
}

/// ⌘-click and ⇧-click pick rows (not shown in a pane); the row menu
/// then offers Send to Selected…, which checks them all.
#[gpui::test]
fn sidebar_rows_are_picked_and_sent_to(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(1, "/w", 1), row(2, "/w", 2), row(3, "/w", 3)],
    );
    let order: Vec<u64> = s.ws.read_with(s.cx, |w, _| {
        w.drawn_sidebar
            .iter()
            .flat_map(|(_, ids)| ids.clone())
            .collect()
    });
    let (first, last) = (order[0], order[2]);
    let cmd = Modifiers::command();
    let shift = Modifiers::shift();
    let at = center(&mut s, "tab-1");
    s.cx.simulate_click(at, cmd);
    s.cx.run_until_parked();
    assert_eq!(
        s.ws.read_with(s.cx, |w, _| w.row_selection().ids().to_vec()),
        [1]
    );
    assert_eq!(shown(&mut s, PaneId(1)), None, "a ⌘-click shows nothing");
    let tab = |id: u64| -> &'static str {
        match id {
            1 => "tab-1",
            2 => "tab-2",
            _ => "tab-3",
        }
    };
    let at = center(&mut s, tab(first));
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert!(s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
    assert_eq!(shown(&mut s, PaneId(1)), Some(first));
    let at = center(&mut s, tab(last));
    s.cx.simulate_click(at, shift);
    s.cx.run_until_parked();
    let picked =
        s.ws.read_with(s.cx, |w, _| w.row_selection().ids().to_vec());
    assert_eq!(picked, order);
    assert!(row_items(&mut s, 2).contains(&RowMenuItem::SendToSelected));
    pick_row(&mut s, 2, RowMenuItem::SendToSelected);
    let sheet = send_sheet(&mut s).expect("send sheet open");
    assert_eq!(
        sheet.read_with(s.cx, |sh, _| sh.form.selected().to_vec()),
        order
    );
    s.cx.simulate_keystrokes("escape");
    // Escape in the sidebar clears the selection.
    click(&mut s, tab(first));
    s.ws.update_in(s.cx, |w, window, cx| w.focus_sidebar(window, cx));
    let at = center(&mut s, tab(last));
    s.cx.simulate_click(at, cmd);
    s.cx.run_until_parked();
    assert!(!s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.row_selection().is_empty()));
}
