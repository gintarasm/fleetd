use super::*;

fn pane_has_keyboard(s: &mut Setup<'_>, pane: PaneId) -> bool {
    let app = s.app.clone();
    s.cx.update(|window, cx| {
        let view = app.read(cx).pane_view(pane).unwrap().clone();
        view.read(cx).focus_handle().is_focused(window)
    })
}

/// Issue #143: Rename, Edit Labels, the kill question and the peek
/// close when their agent is removed (and stay for another agent's
/// removal); the keyboard goes back to the focused pane.
#[gpui::test]
fn a_sheet_closes_when_its_agent_goes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    type Open = fn(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>);
    type IsOpen = fn(&Workspace) -> bool;
    let cases: [(&str, Open, IsOpen); 4] = [
        (
            "rename",
            |w, window, cx| w.open_rename(3, window, cx),
            |w| w.rename_sheet().is_some(),
        ),
        (
            "labels",
            |w, window, cx| w.open_labels(3, window, cx),
            |w| w.labels_sheet().is_some(),
        ),
        (
            "kill",
            |w, window, cx| w.kill(&Kill { agent_id: 3 }, window, cx),
            |w| w.kill_sheet().is_some(),
        ),
        (
            "peek",
            |w, window, cx| w.open_peek(3, window, cx),
            |w| w.peek().is_some(),
        ),
    ];
    for (what, open, is_open) in cases {
        agents(&mut s, &fake, vec![row(3, "/w", 3), row(4, "/w", 4)]);
        s.ws.update_in(s.cx, open);
        s.cx.run_until_parked();
        assert!(s.ws.read_with(s.cx, |w, _| is_open(w)), "{what} opens");
        agents(&mut s, &fake, vec![row(3, "/w", 3)]);
        assert!(
            s.ws.read_with(s.cx, |w, _| is_open(w)),
            "{what} stays for another agent's removal"
        );
        agents(&mut s, &fake, vec![]);
        assert!(
            s.ws.read_with(s.cx, |w, _| !w.has_modal()),
            "{what} closes with its agent"
        );
        assert!(
            pane_has_keyboard(&mut s, PaneId(1)),
            "{what}: keyboard back"
        );
    }
}

/// Issue #143: the kill question for several agents closes when one of
/// them is removed (its list of names would be wrong).
#[gpui::test]
fn the_bulk_kill_question_closes_when_one_of_its_agents_goes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(2, "/w", 2), row(3, "/w", 3), row(4, "/w", 4)],
    );
    s.ws.update_in(s.cx, |w, window, cx| {
        let sheet =
            cx.new(|cx| super::KillSheet::many(vec![2, 3], vec!["2".into(), "3".into()], cx));
        let sub = cx.subscribe_in(&sheet, window, Workspace::kill_event);
        w.modal = Some(super::Modal::Kill(sheet, sub));
    });
    agents(&mut s, &fake, vec![row(2, "/w", 2), row(3, "/w", 3)]);
    assert!(s.ws.read_with(s.cx, |w, _| w.kill_sheet().is_some()));
    agents(&mut s, &fake, vec![row(2, "/w", 2)]);
    assert!(s.ws.read_with(s.cx, |w, _| w.kill_sheet().is_none()));
    assert!(kills(&fake).is_empty());
}

/// Issue #143: the Answer sheet closes when its question closes (the
/// agent reported on, or asked another), or its agent goes.
#[gpui::test]
fn the_answer_sheet_closes_with_its_question(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let open = |s: &mut Setup<'_>| {
        s.ws.update_in(s.cx, |w, window, cx| w.open_answer(3, window, cx));
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_some())
    };
    let still_open = |s: &mut Setup<'_>| s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_some());
    for after in [
        vec![asking(3, 10)],
        vec![row(3, "/w", 3)],
        vec![row(4, "/w", 4)],
    ] {
        agents(&mut s, &fake, vec![asking(3, 9), row(4, "/w", 4)]);
        assert!(open(&mut s));
        // Another agent going changes nothing.
        agents(&mut s, &fake, vec![asking(3, 9)]);
        assert!(still_open(&mut s));
        agents(&mut s, &fake, after);
        assert!(!still_open(&mut s));
        assert!(pane_has_keyboard(&mut s, PaneId(1)));
    }
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Answer(..))));
}
