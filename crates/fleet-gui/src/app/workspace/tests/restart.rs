use super::*;

/// Pane 1 shows agent 5, exited, and cmd-r restarts it in place.
fn restart_in_pane_1(s: &mut Setup<'_>, fake: &Fake) {
    agents(s, fake, vec![ended(5, "/w", 1, AgentState::Exited)]);
    click(s, "tab-5");
    attach_model(fake, PaneId(1), 5);
    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.cx.simulate_keystrokes("cmd-r");
    assert!(
        fake.calls()
            .iter()
            .any(|c| matches!(c, Call::RestartInPane { agent_id: 5, .. }))
    );
}

#[gpui::test]
fn pane_whose_restart_reply_was_lost_attaches_the_successor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    restart_in_pane_1(&mut s, &fake);
    // The control connection names the successor; the pane expects its
    // own reply to show it, so nothing attaches yet.
    s.ui.try_send(UiEvent::AgentRemoved {
        agent_id: 5,
        replaced_by: Some(9),
    })
    .unwrap();
    s.cx.run_until_parked();
    assert!(!fake.calls().contains(&Call::Attach(PaneId(1), 9)));
    // That reply was lost with the pane's connection.
    s.ui.try_send(UiEvent::LaunchDropped(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 9)));
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
}

#[gpui::test]
fn pane_whose_restart_reply_was_lost_follows_a_later_successor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    restart_in_pane_1(&mut s, &fake);
    let attaches = |f: &Fake| {
        f.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Attach(PaneId(1), _)))
            .count()
    };
    let before = attaches(&fake);
    // The reply is lost before the successor is announced: the pane
    // shows the agent its layout names again...
    s.ui.try_send(UiEvent::LaunchDropped(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 5)));
    assert_eq!(attaches(&fake), before + 1);
    // ...and attaches the successor once it is.
    s.ui.try_send(UiEvent::AgentRemoved {
        agent_id: 5,
        replaced_by: Some(9),
    })
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 9)));
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
}

#[gpui::test]
fn agent_removed_moves_pane_to_successor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1), row(7, "/w", 2)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-7");
    s.ui.try_send(UiEvent::AgentRemoved {
        agent_id: 5,
        replaced_by: Some(9),
    })
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    assert_eq!(shown(&mut s, PaneId(2)), Some(7));
    assert!(fake.calls().contains(&Call::Attach(PaneId(1), 9)));
    // Output from the old agent before the attach does not revert it.
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    s.cx.executor().advance_clock(SAVE_DEBOUNCE);
    s.cx.run_until_parked();
    let saved = s.app.read_with(s.cx, |a, _| a.layout().to_saved());
    assert_eq!(GuiState::load(dir.path()).layouts["local"], saved);
    assert_eq!(Layout::from_saved(&saved).agent(PaneId(1)), Some(9));

    // Forgotten: the pane is emptied, on a fresh connection.
    s.ui.try_send(UiEvent::AgentRemoved {
        agent_id: 7,
        replaced_by: None,
    })
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(2)), None);
    assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(2), None)));
    // The focused pane keeps the keyboard though its view was replaced.
    let app = s.app.clone();
    let has_focus = s.cx.update(|window, cx| {
        let view = app.read(cx).pane_view(PaneId(2)).unwrap().clone();
        view.read(cx).focus_handle().is_focused(window)
    });
    assert!(has_focus);
}

#[gpui::test]
fn list_follows_replacement_into_layout(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1), row(6, "/w", 2)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    s.cx.simulate_keystrokes("cmd-d");
    click(&mut s, "tab-6");
    // Agent 6 is gone from the node (a lost attach), 5 was restarted
    // as 9 while the GUI did not hear of it.
    fake.model(PaneId(2)).lock().unwrap().on_lost();
    *fake.replaced.lock().unwrap() = HashMap::from([(5, 9)]);
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    agents(&mut s, &fake, vec![row(9, "/w", 3)]);
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    assert!(fake.calls().contains(&Call::Attach(PaneId(1), 9)));
    assert_eq!(shown(&mut s, PaneId(2)), None);
    assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(2), None)));
}

#[gpui::test]
fn exited_pane_unlisted_by_two_lists_is_emptied(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    list(&mut s, &fake, vec![ended(8, "/w", 1, AgentState::Exited)]);
    click(&mut s, "tab-8");
    attach_model(&fake, PaneId(1), 8);
    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    // Agent 8 is forgotten while the GUI misses the `AgentRemoved`.
    // One list without it might just predate it (a new agent that
    // exited at once): the pane keeps its final screen...
    list(&mut s, &fake, vec![]);
    assert_eq!(shown(&mut s, PaneId(1)), Some(8));
    // ...as it does for events that are not lists.
    agents(&mut s, &fake, vec![]);
    assert_eq!(shown(&mut s, PaneId(1)), Some(8));
    // The next list confirms it is gone.
    list(&mut s, &fake, vec![]);
    assert_eq!(shown(&mut s, PaneId(1)), None);
    assert_eq!(fake.calls().last(), Some(&Call::Open(PaneId(1), None)));
}

#[gpui::test]
fn exited_pane_listed_again_is_kept(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    list(&mut s, &fake, vec![]);
    // A spawn lands and its agent exits before any list shows it.
    spawn_via_sheet(&mut s, "/w");
    attach_model(&fake, PaneId(1), 9);
    fake.model(PaneId(1)).lock().unwrap().on_exited(0);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    list(&mut s, &fake, vec![]);
    list(&mut s, &fake, vec![ended(9, "/w", 1, AgentState::Exited)]);
    // Absent once, then listed: absent once more is not two in a row.
    list(&mut s, &fake, vec![]);
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
}

#[gpui::test]
fn unlisted_pane_with_a_restart_on_its_way_is_kept(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    list(&mut s, &fake, vec![ended(6, "/w", 1, AgentState::Lost)]);
    s.app.update(s.cx, |a, cx| a.show_agent(6, cx));
    fake.model(PaneId(1)).lock().unwrap().on_lost();
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.cx.simulate_keystrokes("cmd-r");
    assert!(
        fake.calls()
            .iter()
            .any(|c| matches!(c, Call::RestartInPane { agent_id: 6, .. }))
    );
    // Lists taken before the restart's successor is known.
    list(&mut s, &fake, vec![]);
    list(&mut s, &fake, vec![]);
    assert_eq!(shown(&mut s, PaneId(1)), Some(6));
}
