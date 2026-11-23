use super::*;

#[gpui::test]
fn row_menu_offers_restart_and_remove_for_exited(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            row(7, "/w", 1),
            ended(8, "/w", 2, AgentState::Exited),
            ended(6, "/w", 3, AgentState::Lost),
        ],
    );
    assert_eq!(
        row_items(&mut s, 7),
        [
            RowMenuItem::OpenSplit,
            RowMenuItem::Rename,
            RowMenuItem::EditLabels,
            RowMenuItem::Duplicate,
            RowMenuItem::SendTo,
            RowMenuItem::SaveOutput,
            RowMenuItem::OpenOutput,
            RowMenuItem::Kill
        ]
    );
    for id in [8, 6] {
        assert_eq!(
            row_items(&mut s, id),
            [
                RowMenuItem::OpenSplit,
                RowMenuItem::Rename,
                RowMenuItem::EditLabels,
                RowMenuItem::Duplicate,
                RowMenuItem::SaveOutput,
                RowMenuItem::OpenOutput,
                RowMenuItem::Restart,
                RowMenuItem::Remove
            ]
        );
    }
    // Restart asks for nothing: it goes out at once.
    pick_row(&mut s, 6, RowMenuItem::Restart);
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(fake.calls().last(), Some(&Call::Restart(6)));
    // Kill asks first.
    pick_row(&mut s, 7, RowMenuItem::Kill);
    assert_eq!(kill_asked(&mut s), Some(7));
}

/// Selects `ids` in the sidebar, as ⌘-clicks do.
fn select_rows(s: &mut Setup<'_>, ids: &[u64]) {
    s.ws.update(s.cx, |w, _| {
        for &id in ids {
            w.row_selection.click(id, true, false, ids, None);
        }
    });
}

#[gpui::test]
fn bulk_menu_items_come_with_a_selection_of_several(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
    assert!(!row_items(&mut s, 1).contains(&RowMenuItem::KillSelected));
    select_rows(&mut s, &[1, 2]);
    let items = row_items(&mut s, 1);
    assert!(items.contains(&RowMenuItem::KillSelected));
    assert!(items.contains(&RowMenuItem::RestartSelected));
    assert!(items.contains(&RowMenuItem::RemoveSelected));
}

#[gpui::test]
fn kill_selected_asks_once_and_kills_only_the_running(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            row(1, "/a", 1),
            row(2, "/b", 2),
            ended(3, "/c", 3, AgentState::Exited),
        ],
    );
    select_rows(&mut s, &[1, 2, 3]);
    pick_row(&mut s, 1, RowMenuItem::KillSelected);
    let question = s.ws.read_with(s.cx, |w, cx| {
        w.kill_sheet().unwrap().read(cx).question.to_string()
    });
    assert_eq!(question, "Kill 2 agents?");
    assert!(kills(&fake).is_empty());
    click(&mut s, "kill-confirm");
    assert_eq!(kill_asked(&mut s), None);
    assert_eq!(kills(&fake), [1, 2]);
}

#[gpui::test]
fn restart_and_remove_selected_act_on_the_exited(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            row(1, "/a", 1),
            ended(2, "/b", 2, AgentState::Exited),
            ended(3, "/c", 3, AgentState::Lost),
        ],
    );
    select_rows(&mut s, &[1, 2, 3]);
    pick_row(&mut s, 1, RowMenuItem::RestartSelected);
    let restarts: Vec<_> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Restart(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(restarts, [2, 3]);
    pick_row(&mut s, 1, RowMenuItem::RemoveSelected);
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![2, 3])));
    assert_eq!(kill_asked(&mut s), None);
}

#[gpui::test]
fn row_menu_open_in_new_split_adds_a_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    pick_row(&mut s, 7, RowMenuItem::OpenSplit);
    assert_eq!(panes(&mut s).len(), 2);
    let pane = focused(&mut s);
    assert_eq!(shown(&mut s, pane), Some(7));
    // With four panes the menu no longer offers it.
    s.cx.simulate_keystrokes("cmd-d cmd-d");
    assert_eq!(
        row_items(&mut s, 7),
        [
            RowMenuItem::Rename,
            RowMenuItem::EditLabels,
            RowMenuItem::Duplicate,
            RowMenuItem::SendTo,
            RowMenuItem::SaveOutput,
            RowMenuItem::OpenOutput,
            RowMenuItem::Kill
        ]
    );
}

#[gpui::test]
fn row_menu_pick_is_checked_against_the_agent_now(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    // The menu offered Kill; the agent exited before the pick.
    agents(&mut s, &fake, vec![ended(7, "/w", 1, AgentState::Exited)]);
    pick_row(&mut s, 7, RowMenuItem::Kill);
    assert_eq!(kill_asked(&mut s), None, "no Kill for an exited agent");
    // Remove picked for an agent the list now shows running.
    agents(&mut s, &fake, vec![row(8, "/w", 2)]);
    pick_row(&mut s, 8, RowMenuItem::Remove);
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
    // An unknown agent offers no Restart either.
    pick_row(&mut s, 99, RowMenuItem::Restart);
    assert!(!fake.calls().contains(&Call::Restart(99)));
}

#[gpui::test]
fn pane_menu_restart_restarts_the_ended_agent_in_place(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::Restart, epoch, window, cx);
    });
    s.cx.run_until_parked();
    let size = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
    });
    assert!(fake.calls().contains(&Call::RestartInPane {
        pane: PaneId(1),
        agent_id: 5,
        size,
    }));
}

#[gpui::test]
fn pane_menu_picks_act_on_their_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-d");
    let [first, second] = panes(&mut s)[..] else {
        panic!("two panes")
    };
    let pick = |s: &mut Setup<'_>, pane, item| {
        s.ws.update_in(s.cx, |ws, window, cx| {
            let epoch = ws.epoch;
            ws.pane_menu_pick(pane, item, epoch, window, cx);
        });
        s.cx.run_until_parked();
    };
    pick(&mut s, first, PaneMenuItem::Maximize);
    assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), Some(first));
    pick(&mut s, first, PaneMenuItem::Restore);
    assert_eq!(s.app.read_with(s.cx, |a, _| a.maximized()), None);
    pick(&mut s, second, PaneMenuItem::Close);
    assert_eq!(panes(&mut s), [first]);
    pick(&mut s, first, PaneMenuItem::SplitDown);
    assert_eq!(panes(&mut s).len(), 2);
}

#[gpui::test]
fn remove_calls_forget(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
    pick_row(&mut s, 8, RowMenuItem::Remove);
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8])));
}

#[gpui::test]
fn cmd_r_restarts_exited_pane_in_place(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    let restarts = |f: &Fake| {
        f.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::RestartInPane { .. }))
            .collect::<Vec<_>>()
    };
    // Live: cmd-r does nothing, and there is no Restart button.
    s.cx.simulate_keystrokes("cmd-r");
    assert!(restarts(&fake).is_empty());
    assert!(!drawn(&mut s, "pane-1-restart"));

    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.cx.simulate_keystrokes("cmd-r");
    let size = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
    });
    let want = Call::RestartInPane {
        pane: PaneId(1),
        agent_id: 5,
        size,
    };
    assert_eq!(restarts(&fake), std::slice::from_ref(&want));
    // The exit banner's button does the same.
    click(&mut s, "pane-1-restart");
    assert_eq!(restarts(&fake), [want.clone(), want]);

    // The pane started the new agent itself: it is not attached again
    // when the control connection names the successor.
    s.ui.try_send(UiEvent::AgentRemoved {
        agent_id: 5,
        replaced_by: Some(9),
    })
    .unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
    assert!(!fake.calls().contains(&Call::Attach(PaneId(1), 9)));
    // Output from the old agent before the reply does not revert it.
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s, PaneId(1)), Some(9));
}

#[gpui::test]
fn ended_pane_summary_copies_its_last_output_and_removes_its_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    fake.model(PaneId(1))
        .lock()
        .unwrap()
        .on_output(b"$ make\r\nbuild failed   \r\n");
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    // Running: no summary.
    assert!(!drawn(&mut s, "pane-1-ended"));
    assert!(!drawn(&mut s, "pane-1-remove"));

    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    let mut exited = ended(5, "/w", 1_000, AgentState::Exited);
    exited.info.exited_unix_ms = Some(193_000);
    agents(&mut s, &fake, vec![exited]);
    assert!(drawn(&mut s, "pane-1-ended"));
    // The bar is told how long the agent ran.
    let run_time = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).chrome.run_time
    });
    assert_eq!(run_time, Some(Duration::from_secs(192)));

    s.cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    click(&mut s, "pane-1-copy-output");
    let clip = s.cx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(clip.as_deref(), Some("$ make\nbuild failed"));

    click(&mut s, "pane-1-remove");
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![5])));
}
