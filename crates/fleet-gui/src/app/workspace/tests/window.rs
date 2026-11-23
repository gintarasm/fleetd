use super::*;

#[gpui::test]
fn update_button_restarts_once_and_outcomes_show_as_toasts(cx: &mut TestAppContext) {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let s = setup(cx, &fake, dir.path());
    let shown = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
    let button = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.update_button());
    assert!(s.cx.debug_bounds("fleetd-update").is_none());
    assert_eq!(button(&s), None);

    // A newer binary is noticed on the next connect: the bar offers
    // Update fleetd.
    fake.update_available.store(true, Ordering::SeqCst);
    s.ui.try_send(UiEvent::Connection(ConnState::Connected))
        .unwrap();
    s.cx.run_until_parked();
    assert_eq!(button(&s).map(|b| b.label), Some("Update fleetd"));
    let update = s.cx.debug_bounds("fleetd-update").unwrap();
    let bar = s.cx.debug_bounds("titlebar").unwrap();
    assert!(bar.contains(&update.center()), "the button is in the bar");

    // A double click asks the session once; the button says it is
    // running and does nothing more.
    s.cx.simulate_click(update.center(), Modifiers::none());
    s.cx.run_until_parked();
    s.cx.simulate_click(update.center(), Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(fake.daemon_restarts.load(Ordering::SeqCst), 1);
    assert_eq!(
        button(&s),
        Some(crate::core::titlebar::UpdateButton {
            label: "Updating fleetd…",
            enabled: false
        })
    );

    // After the restart the binary matches again (gpui keeps the bounds
    // of elements it once drew, so the app's state is what is checked);
    // the toast says what happened, in the CLI's words.
    fake.restart_pending.store(false, Ordering::SeqCst);
    fake.update_available.store(false, Ordering::SeqCst);
    s.ui.try_send(UiEvent::DaemonRestarted(
        fleet_client::RestartOutcome::Restarted { kept: 2 },
    ))
    .unwrap();
    s.cx.run_until_parked();
    // Success is a neutral notice, not an error.
    assert_eq!(shown(&s), None);
    let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
    assert_eq!(notice.as_deref(), Some("fleetd restarted: 2 agents kept"));
    assert!(s.cx.debug_bounds("error-toast").is_some());
    assert!(!s.app.read_with(s.cx, |a, _| a.update_available));
    assert_eq!(button(&s), None, "the button goes away");
    s.ui.try_send(UiEvent::DaemonRestarted(
        fleet_client::RestartOutcome::Partial {
            kept: 1,
            ended: 1,
            ids: Some(vec![3]),
        },
    ))
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(
        shown(&s).as_deref(),
        Some("fleetd restarted: 1 kept, 1 could not be restored and were ended (ids 3)")
    );
    s.ui.try_send(UiEvent::DaemonRestartFailed("exec failed".into()))
        .unwrap();
    s.cx.run_until_parked();
    assert_eq!(
        shown(&s).as_deref(),
        Some("fleetd restart failed: exec failed; the old fleetd is still running")
    );
    s.ui.try_send(UiEvent::DaemonRestartFailed(
        fleet_client::did_not_come_back(),
    ))
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(
        shown(&s).as_deref(),
        Some("fleetd did not come back within 10 s")
    );
}

#[gpui::test]
fn new_agent_sheet_is_as_tall_as_its_content(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-n");
    s.cx.run_until_parked();
    let sheet = s.cx.debug_bounds("new-agent-sheet").unwrap();
    let window = s.cx.update(|window, _| window.viewport_size());
    assert!(
        sheet.size.height < window.height / 2.0,
        "sheet height {:?} of {:?}",
        sheet.size.height,
        window.height
    );
}

#[gpui::test]
fn reconnecting_clears_the_connection_error_only(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let s = setup(cx, &fake, dir.path());
    let shown = |s: &Setup<'_>| s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));

    s.ui.try_send(UiEvent::ConnectionError(
        "fleetd is not running on this Mac".into(),
    ))
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(
        shown(&s).as_deref(),
        Some("fleetd is not running on this Mac")
    );
    s.ui.try_send(UiEvent::Connection(ConnState::Connected))
        .unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&s), None);
    assert!(s.cx.debug_bounds("error-toast").is_none());

    // Any other error outlives a reconnect.
    s.ui.try_send(UiEvent::Error("spawn failed: no such directory".into()))
        .unwrap();
    s.ui.try_send(UiEvent::Connection(ConnState::Connected))
        .unwrap();
    s.cx.run_until_parked();
    assert_eq!(
        shown(&s).as_deref(),
        Some("spawn failed: no such directory")
    );
}

#[gpui::test]
fn click_in_sheet_over_a_pane_does_not_reach_the_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    {
        let model = fake.model(PaneId(1));
        let mut m = model.lock().unwrap();
        m.on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            history_tail: (0..50)
                .map(|i| format!("h{i}").into_bytes().into())
                .collect(),
            ..Default::default()
        });
        // Mouse reporting on: a click reaching the pane would be sent.
        m.on_output(b"hello\x1b[?1000h\x1b[?1006h");
    }
    s.cx.simulate_keystrokes("cmd-n");
    let at = center(&mut s, "new-agent-sheet");
    let grid = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).layout.unwrap()
    });
    assert!(
        at.x > grid.origin.x && at.y > grid.origin.y,
        "the sheet is over the pane"
    );
    fake.inputs.lock().unwrap().clear();
    s.cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    s.cx.simulate_event(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Lines(point(0.0, 3.0)),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    // Shift-drag would select if it reached the pane.
    s.cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::shift());
    s.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::shift());
    s.cx.run_until_parked();
    assert!(fake.inputs.lock().unwrap().is_empty());
    let model = fake.model(PaneId(1));
    let m = model.lock().unwrap();
    assert_eq!(m.selection_text(), None);
    assert_eq!(m.scroll_offset(), 0);
}

#[gpui::test]
fn layout_persists_per_target(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
    click(&mut s, "tab-7");
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-8");
    let saved = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
    // Debounced: nothing written yet.
    assert!(GuiState::load(dir.path()).layouts.is_empty());
    s.cx.executor().advance_clock(SAVE_DEBOUNCE);
    s.cx.run_until_parked();
    assert_eq!(GuiState::load(dir.path()).layouts["local"], saved);

    // A new app restores it and re-attaches each pane's agent.
    let fake2 = Arc::new(Fake::default());
    let app2 = app_state(s.cx, &fake2, dir.path());
    assert_eq!(
        fake2.calls(),
        [
            Call::Open(PaneId(1), Some(7)),
            Call::Open(PaneId(2), Some(8))
        ]
    );
    let restored = app2.read_with(s.cx, |a, _| a.layout().to_saved());
    assert_eq!(restored, saved);
}

#[gpui::test]
fn closing_the_window_keeps_the_panes_attached(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-d");
    s.cx.update(|window, _| window.remove_window());
    s.cx.run_until_parked();
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Close(_))));
    let alive = s.app.read_with(s.cx, |a, _| {
        a.layout().panes().iter().all(|p| a.pane_view(*p).is_some())
    });
    assert!(alive);
}

#[gpui::test]
fn switching_target_closes_the_sheet_and_drops_stale_menu_picks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
    s.cx.simulate_keystrokes("cmd-d cmd-n");
    let (open, epoch) =
        s.ws.read_with(s.cx, |ws, _| (ws.sheet().is_some(), ws.epoch));
    assert!(open);
    // A direct switch (not through the connect sheet).
    let other = Arc::new(Fake::default());
    let session: Arc<dyn SessionApi> = other.clone();
    let (_tx, rx) = async_channel::unbounded();
    let target = Target::Ssh {
        dest: "me@box".into(),
    };
    s.app
        .update(s.cx, |a, cx| a.replace_session(session, rx, target, cx));
    s.cx.run_until_parked();
    let closed = s.ws.read_with(s.cx, |ws, _| ws.sheet().is_none());
    assert!(closed, "the sheet targeted the old layout's panes");
    // A pick from a menu opened before the switch does nothing.
    s.ws.update_in(s.cx, |ws, window, cx| {
        ws.row_menu_pick(7, RowMenuItem::Remove, epoch, window, cx);
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::Close, epoch, window, cx);
    });
    s.cx.run_until_parked();
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
    assert!(!other.calls().contains(&Call::Forget(vec![7])));
}

#[gpui::test]
fn maximize_toggles_one_pane_and_keeps_the_others(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let drawn = |s: &mut Setup<'_>| {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, _| w.drawn_panes().to_vec())
    };
    s.cx.simulate_keystrokes("cmd-d");
    assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
    s.cx.simulate_keystrokes("cmd-shift-enter");
    assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), Some(PaneId(2)));
    assert_eq!(drawn(&mut s), [PaneId(2)]);
    // The hidden pane stays attached.
    assert_eq!(panes(&mut s).len(), 2);
    assert!(
        s.app
            .read_with(s.cx, |a, _| a.pane_view(PaneId(1)).is_some())
    );
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Close(_))));
    // The header button toggles back.
    click(&mut s, "pane-2-maximize");
    assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), None);
    assert_eq!(drawn(&mut s), [PaneId(1), PaneId(2)]);
    // And maximizes too.
    click(&mut s, "pane-1-maximize");
    assert_eq!(drawn(&mut s), [PaneId(1)]);
}

#[gpui::test]
fn header_buttons_split_and_close_their_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    click(&mut s, "pane-1-split-right");
    assert_eq!(panes(&mut s), [PaneId(1), PaneId(2)]);
    click(&mut s, "pane-2-split-down");
    assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(3)]);
    // Pane 1's button acts on pane 1, though pane 3 has the focus.
    click(&mut s, "pane-1-split-down");
    // Numbered in reading order: the new bottom-left pane (id 4) is 3.
    assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(4), PaneId(3)]);
    // Selectors go by pane number: number 3 is pane id 4.
    click(&mut s, "pane-3-close");
    assert_eq!(panes(&mut s), [PaneId(1), PaneId(2), PaneId(3)]);
    assert_eq!(fake.calls().last(), Some(&Call::Close(PaneId(4))));
}

#[gpui::test]
fn focused_tab_highlight_follows_the_focused_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(7, "/w", 1), row(8, "/w", 2), row(9, "/w", 3)],
    );
    click(&mut s, "tab-7");
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-8");
    let marks = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.tab_marks());
    let m = marks(&mut s);
    assert_eq!(m.get(&8), Some(&TabMark::Focused));
    assert_eq!(m.get(&7), Some(&TabMark::Shown));
    assert_eq!(m.get(&9), None);
    s.cx.simulate_keystrokes("cmd-1");
    let m = marks(&mut s);
    assert_eq!(m.get(&7), Some(&TabMark::Focused));
    assert_eq!(m.get(&8), Some(&TabMark::Shown));
    // Clicking into a pane moves the highlight too.
    let grid = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(2)).unwrap().read(cx).layout.unwrap()
    });
    let at = point(
        grid.origin.x + grid.cell_w * 2.0,
        grid.origin.y + grid.line_h * 2.0,
    );
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    let app = s.app.clone();
    let f = s.cx.update(|window, cx| {
        let view = app.read(cx).pane_view(PaneId(2)).unwrap().clone();
        view.read(cx).focus_handle().is_focused(window)
    });
    assert!(f, "pane 2 focused by the click");
    assert_eq!(marks(&mut s).get(&8), Some(&TabMark::Focused));
}

#[gpui::test]
fn bar_new_agent_button_opens_the_sheet_and_the_title_counts_agents(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
    );
    assert!(drawn(&mut s, "titlebar") && drawn(&mut s, "bar-target"));
    let title = s.ws.read_with(s.cx, |ws, _| ws.window_title.clone());
    assert_eq!(title, "fleet — This Mac");
    // The bar sits above the sidebar's first tab.
    let bar = s.cx.debug_bounds("titlebar").unwrap();
    let tab = s.cx.debug_bounds("tab-7").unwrap();
    assert!(bar.bottom() <= tab.top());
    click(&mut s, "new-agent");
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
}

#[gpui::test]
fn the_bar_stays_a_drag_strip_under_a_sheet(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
    let bar = s.cx.debug_bounds("titlebar").unwrap();
    let clear = center(&mut s, "clear-exited");
    s.cx.simulate_keystrokes("cmd-n");
    assert!(drawn(&mut s, "sheet-drag-strip"));
    let strip = s.cx.debug_bounds("sheet-drag-strip").unwrap();
    assert_eq!(
        (strip.top(), strip.size.height),
        (bar.top(), bar.size.height)
    );
    assert!(strip.size.width >= bar.size.width);
    // The bar's buttons are inert under it; the sheet stays.
    s.cx.simulate_click(clear, Modifiers::none());
    s.cx.run_until_parked();
    assert!(!fake.calls().contains(&Call::ClearExited));
    assert!(s.ws.read_with(s.cx, |ws, _| ws.sheet().is_some()));
}
