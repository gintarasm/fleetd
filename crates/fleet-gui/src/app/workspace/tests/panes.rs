use super::*;

#[gpui::test]
fn files_dropped_on_a_row_are_typed_into_its_agent(cx: &mut TestAppContext) {
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let mut s = setup(cx, &fake, dir.path());
    let rows = vec![
        row(1, "/w", 1),
        row(2, "/w", 2),
        ended(3, "/w", 3, AgentState::Exited),
    ];
    agents(&mut s, &fake, rows);
    s.ws.update_in(s.cx, |w, window, cx| w.show_agent(1, window, cx));
    s.cx.run_until_parked();
    let pane = focused(&mut s);
    fake.model(pane).lock().unwrap().on_output(b"\x1b[?2004h");
    fake.inputs.lock().unwrap().clear();
    let paths = vec!["/a b.png".to_owned(), "/c".to_owned()];

    // Shown in a pane: through it, bracketed as the agent asked.
    s.app
        .update(s.cx, |a, cx| a.drop_paths_on_agent(1, &paths, cx));
    let typed = b"\x1b[200~'/a b.png' /c\x1b[201~".to_vec();
    assert_eq!(*fake.inputs.lock().unwrap(), vec![typed]);

    // Not shown: on the control connection.
    s.app
        .update(s.cx, |a, cx| a.drop_paths_on_agent(2, &paths, cx));
    let sent = Call::Send(2, b"'/a b.png' /c".to_vec());
    assert!(fake.calls().contains(&sent), "{:?}", fake.calls());

    // An ended agent takes nothing.
    let before = fake.calls().len();
    s.app
        .update(s.cx, |a, cx| a.drop_paths_on_agent(3, &paths, cx));
    assert_eq!(fake.calls().len(), before);
    assert_eq!(fake.inputs.lock().unwrap().len(), 1);
}

#[gpui::test]
fn a_pane_notice_shows_in_the_toast(cx: &mut TestAppContext) {
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let mut s = setup(cx, &fake, dir.path());
    let pane = focused(&mut s);
    let view = s
        .app
        .read_with(s.cx, |a, _| a.pane_view(pane).cloned())
        .unwrap();
    view.update(s.cx, |v, cx| {
        v.chrome.has_agent = true;
        v.chrome.local = false;
        v.type_paths(&["/a".to_owned()], cx);
    });
    s.cx.run_until_parked();
    let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
    assert_eq!(
        notice.as_deref(),
        Some(crate::core::drop_paste::DROP_NEEDS_LOCAL)
    );
}

#[gpui::test]
fn files_dropped_on_a_remote_agent_show_a_notice(cx: &mut TestAppContext) {
    let fake = Arc::new(Fake::default());
    *fake.agents.lock().unwrap() = vec![row(1, "/w", 1)];
    let dir = tempfile::tempdir().unwrap();
    let (_tx, rx) = async_channel::unbounded();
    let session: Arc<dyn SessionApi> = fake.clone();
    let target = Target::Ssh { dest: "box".into() };
    let path = dir.path().to_owned();
    let app = cx.new(|cx| {
        AppState::new(
            session,
            rx,
            target,
            path,
            GuiConfig::default(),
            Theme::dark(),
            cx,
        )
    });
    let paths = vec!["/a".to_owned()];
    app.update(cx, |a, cx| a.drop_paths_on_agent(1, &paths, cx));
    let notice = app.read_with(cx, |a, _| a.notice().map(str::to_owned));
    assert_eq!(
        notice.as_deref(),
        Some(crate::core::drop_paste::DROP_NEEDS_LOCAL)
    );
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Send(..))));
    assert!(fake.inputs.lock().unwrap().is_empty());
}

#[gpui::test]
fn agents_listed_before_the_window_are_in_its_first_frame(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let app = app_state(cx, &fake, dir.path());
    assert!(!app.read_with(cx, |a, _| a.has_agents()));
    // The list arrives while the window opens, its event not yet applied.
    *fake.agents.lock().unwrap() = vec![row(1, "/a", 1), row(2, "/b", 2)];
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    let state = app.clone();
    let (_ws, cx) = cx.add_window_view(|window, cx| Workspace::new(state, window, cx));
    let ids: Vec<u64> = app.read_with(cx, |a, _| a.agents.iter().map(|r| r.info.id).collect());
    assert_eq!(ids, [1, 2]);
    cx.run_until_parked();
    assert!(cx.debug_bounds("tab-1").is_some() && cx.debug_bounds("tab-2").is_some());
}

#[gpui::test]
fn cmd_d_splits_and_cmd_w_detaches(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    assert_eq!(panes(&mut s).len(), 1);
    s.cx.simulate_keystrokes("cmd-d");
    let p = panes(&mut s);
    assert_eq!(p, [PaneId(1), PaneId(2)]);
    assert_eq!(focused(&mut s), PaneId(2));
    s.cx.simulate_keystrokes("cmd-w");
    assert_eq!(panes(&mut s), [PaneId(1)]);
    let calls = fake.calls();
    assert!(calls.contains(&Call::Close(PaneId(2))), "{calls:?}");
    assert!(!calls.iter().any(|c| matches!(c, Call::Kill(_))));
    // cmd-shift-d splits down.
    s.cx.simulate_keystrokes("cmd-shift-d");
    assert_eq!(panes(&mut s).len(), 2);
}

#[gpui::test]
fn fifth_split_is_refused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-d cmd-shift-d cmd-d");
    assert_eq!(panes(&mut s).len(), 4);
    let opens = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|c| matches!(c, Call::Open(..)))
            .count()
    };
    let before = opens(&fake);
    s.cx.simulate_keystrokes("cmd-d");
    assert_eq!(panes(&mut s).len(), 4);
    assert_eq!(opens(&fake), before);
}

#[gpui::test]
fn cmd_number_focuses_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-d cmd-shift-d");
    let p = panes(&mut s);
    assert_eq!(p.len(), 3);
    for (n, pane) in p.iter().enumerate() {
        s.cx.simulate_keystrokes(&format!("cmd-{}", n + 1));
        assert_eq!(focused(&mut s), *pane);
        let app = s.app.clone();
        // Within the pane: the new ones' split choosers have it.
        let has_focus = s.cx.update(|window, cx| {
            let view = app.read(cx).pane_view(*pane).unwrap().clone();
            view.read(cx).focus_handle().contains_focused(window, cx)
        });
        assert!(has_focus, "pane {n} has keyboard focus");
    }
    // Arrows move between neighbours: 1 | (2 over 3).
    s.cx.simulate_keystrokes("cmd-1 cmd-alt-right");
    assert_eq!(focused(&mut s), p[1]);
    s.cx.simulate_keystrokes("cmd-alt-down");
    assert_eq!(focused(&mut s), p[2]);
    s.cx.simulate_keystrokes("cmd-4");
    assert_eq!(focused(&mut s), p[2]);
}

#[gpui::test]
fn clicking_sidebar_row_attaches_in_focused_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.simulate_keystrokes("cmd-d cmd-1");
    click(&mut s, "tab-7");
    assert!(fake.calls().contains(&Call::Attach(PaneId(1), 7)));
    let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
    assert_eq!(shown, Some(7));
    assert_eq!(focused(&mut s), PaneId(1));
}

#[gpui::test]
fn clicking_the_shown_agent_reattaches_a_pane_not_showing_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    let attaches = |f: &Fake| {
        f.calls()
            .iter()
            .filter(|c| **c == Call::Attach(PaneId(1), 7))
            .count()
    };
    click(&mut s, "tab-7");
    assert_eq!(attaches(&fake), 1);
    // The layout names agent 7 but the pane does not show it (a failed
    // spawn left it attaching): clicking attaches again.
    click(&mut s, "tab-7");
    assert_eq!(attaches(&fake), 2);
    // Shown: clicking again does nothing.
    attach_model(&fake, PaneId(1), 7);
    click(&mut s, "tab-7");
    assert_eq!(attaches(&fake), 2);
}
