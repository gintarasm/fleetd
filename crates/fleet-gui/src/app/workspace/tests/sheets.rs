use super::*;

/// Rows show their labels as chips (not the name, project or
/// `fleet.*`), and the pane header the name with `program · id`.
#[gpui::test]
fn rows_and_headers_show_names_and_chips(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let labels = [
        ("role", "reviewer"),
        ("project", "p"),
        ("fleet.parent", "2"),
    ];
    agents(
        &mut s,
        &fake,
        vec![named(1, "lto-falcon", &labels), row(2, "/w", 2)],
    );
    assert!(s.cx.debug_bounds("tab-1-name").is_some());
    assert!(s.cx.debug_bounds("tab-1-detail").is_some());
    assert!(s.cx.debug_bounds("tab-1-chip-role").is_some());
    for hidden in [
        "tab-1-chip-name",
        "tab-1-chip-project",
        "tab-1-chip-fleet.parent",
    ] {
        assert!(s.cx.debug_bounds(hidden).is_none(), "{hidden}");
    }
    assert!(s.cx.debug_bounds("tab-2-chip-role").is_none());
    click(&mut s, "tab-1");
    attach_model(&fake, PaneId(1), 1);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    let chrome = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).chrome.clone()
    });
    assert_eq!(chrome.name.as_deref(), Some("lto-falcon"));
    assert_eq!(chrome.agent, Some(1));
    assert!(s.cx.debug_bounds("pane-1-name").is_some());
    assert!(s.cx.debug_bounds("pane-1-detail").is_some());
}

/// ⌘J (ADR 0100): shows the agents that need you in turn, wrapping;
/// with none, a short toast and the pane unchanged.
#[gpui::test]
fn cmd_j_walks_the_agents_that_need_you(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let shown = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.layout().agent(a.focused()));
    agents(&mut s, &fake, vec![row(2, "/w", 2)]);
    s.cx.simulate_keystrokes("cmd-j");
    assert_eq!(
        s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned)),
        Some("No agent needs you".to_owned())
    );
    assert_ne!(shown(&mut s), Some(2));

    agents(
        &mut s,
        &fake,
        vec![row(2, "/w", 2), asking(3, 9), asking(4, 10)],
    );
    s.cx.simulate_keystrokes("cmd-j");
    assert_eq!(shown(&mut s), Some(3));
    s.cx.simulate_keystrokes("cmd-j");
    assert_eq!(shown(&mut s), Some(4));
    s.cx.simulate_keystrokes("cmd-j");
    assert_eq!(shown(&mut s), Some(3));
}

/// A question (ADR 0078): choices answer from the row and from the
/// pane header, Reply… opens the sheet for free text, and a question
/// that is closed (or an ended agent's) shows nothing.
#[gpui::test]
fn a_question_is_answered_from_the_row_the_header_and_the_sheet(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![asking(3, 9), row(4, "/w", 4)]);
    assert!(s.cx.debug_bounds("row-question-3-Which branch?").is_some());
    assert!(s.cx.debug_bounds("row-reply-4").is_none());
    click(&mut s, "row-choice-3-dev");
    assert!(fake.calls().contains(&Call::Answer(3, 9, "dev".into())));
    // Not also the row's click: no pane took the agent.
    assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

    // Reply…: the sheet with the text field.
    click(&mut s, "row-reply-3");
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.answer_sheet().cloned())
            .expect("answer sheet open");
    // Nothing typed: refused, still open.
    s.cx.simulate_keystrokes("enter");
    assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
    let field = sheet.read_with(s.cx, |sh, _| sh.reply.clone());
    field.update(s.cx, |f, cx| f.set_text("something else".into(), cx));
    s.cx.simulate_keystrokes("enter");
    assert!(
        fake.calls()
            .contains(&Call::Answer(3, 9, "something else".into()))
    );
    assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

    // A choice in the sheet; Escape cancels.
    click(&mut s, "row-reply-3");
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.answer_sheet().is_none()));

    // The pane header strip.
    attach_model(&fake, PaneId(1), 3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    click(&mut s, "pane-choice-3-main");
    assert!(fake.calls().contains(&Call::Answer(3, 9, "main".into())));
}

/// A question that was answered (the agent reported on) or whose agent
/// ended shows nothing.
#[gpui::test]
fn a_closed_question_shows_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![asking(3, 9), asking(4, 10)]);
    assert!(s.cx.debug_bounds("row-reply-3").is_some());
    assert!(s.cx.debug_bounds("row-reply-4").is_some());
    let mut gone = asking(3, 9);
    gone.info.set_state(AgentState::Exited);
    agents(&mut s, &fake, vec![gone, row(4, "/w", 4)]);
    assert!(s.cx.debug_bounds("row-reply-3").is_none());
    assert!(s.cx.debug_bounds("row-reply-4").is_none());
}

/// Rename: a double-click on the name in the row or the header, or
/// Rename… in the row and pane menus.
#[gpui::test]
fn rename_from_row_header_and_menus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![named(3, "old-name", &[])]);
    double_click(&mut s, "tab-3-name");
    let sheet = s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned());
    let text = sheet
        .expect("rename sheet open")
        .read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
    assert_eq!(text, "old-name");
    rename_to(&mut s, "  new-name ");
    assert!(fake.calls().contains(&Call::Rename(3, "new-name".into())));
    assert!(s.ws.read_with(s.cx, |w, _| w.rename_sheet().is_none()));

    // A bad name keeps the sheet open; Escape cancels.
    pick_row(&mut s, 3, RowMenuItem::Rename);
    rename_to(&mut s, "123");
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.rename_sheet().cloned())
            .unwrap();
    assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.rename_sheet().is_none()));

    // The header: shown in pane 1.
    attach_model(&fake, PaneId(1), 3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    double_click(&mut s, "pane-1-name");
    rename_to(&mut s, "from-header");
    assert!(
        fake.calls()
            .contains(&Call::Rename(3, "from-header".into()))
    );
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::Rename, epoch, window, cx);
    });
    rename_to(&mut s, "from-menu");
    assert!(fake.calls().contains(&Call::Rename(3, "from-menu".into())));
}

fn label_rows(s: &mut Setup<'_>) -> Vec<String> {
    labels_sheet(s).read_with(s.cx, |sh, cx| {
        sh.rows
            .iter()
            .map(|r| r.read(cx).text().to_owned())
            .collect()
    })
}

fn set_label_row(s: &mut Setup<'_>, index: usize, text: &str) {
    let field = labels_sheet(s).read_with(s.cx, |sh, _| sh.rows[index].clone());
    let text = text.to_owned();
    field.update(s.cx, |f, cx| f.set_text(text, cx));
}

/// Edit Labels… in the row and pane menus: the editable labels as
/// rows (not the name or `fleet.*`); add, edit and remove go out as
/// one `SetLabels`.
#[gpui::test]
fn edit_labels_from_row_and_pane_menus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let labels = [("role", "coder"), ("team", "core"), ("fleet.parent", "2")];
    agents(&mut s, &fake, vec![named(3, "otter", &labels)]);
    pick_row(&mut s, 3, RowMenuItem::EditLabels);
    assert_eq!(label_rows(&mut s), ["role=coder", "team=core"]);
    assert!(s.cx.debug_bounds("labels-sheet").is_some());

    // Edit one, remove one, add one; Return saves.
    set_label_row(&mut s, 0, "role = reviewer");
    click(&mut s, "label-remove-1");
    assert_eq!(label_rows(&mut s), ["role = reviewer"]);
    labels_sheet(&mut s).update_in(s.cx, |sh, window, cx| sh.add_row(window, cx));
    set_label_row(&mut s, 1, "new=1");
    s.cx.simulate_keystrokes("enter");
    assert_eq!(
        fake.calls().last(),
        Some(&Call::SetLabels(
            3,
            vec![
                ("new".into(), "1".into()),
                ("role".into(), "reviewer".into())
            ],
            vec!["team".into()]
        ))
    );
    assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));

    // A bad row keeps the sheet open; Escape cancels; no change sends
    // nothing.
    let before = fake.calls().len();
    pick_row(&mut s, 3, RowMenuItem::EditLabels);
    set_label_row(&mut s, 0, "name=x");
    s.cx.simulate_keystrokes("enter");
    let sheet = labels_sheet(&mut s);
    assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));
    pick_row(&mut s, 3, RowMenuItem::EditLabels);
    s.cx.simulate_keystrokes("enter");
    assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_none()));
    assert_eq!(fake.calls().len(), before);

    // The pane menu: shown in pane 1.
    attach_model(&fake, PaneId(1), 3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::EditLabels, epoch, window, cx);
    });
    s.cx.run_until_parked();
    assert_eq!(label_rows(&mut s), ["role=coder", "team=core"]);
}

/// A click on a label chip filters the sidebar by it; a second click
/// clears it.
#[gpui::test]
fn a_label_chip_filters_the_sidebar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            named(1, "otter", &[("role", "reviewer")]),
            named(2, "falcon", &[("role", "coder")]),
        ],
    );
    click(&mut s, "tab-1-chip-role");
    let query = |s: &mut Setup<'_>| {
        s.ws.read_with(s.cx, |w, cx| w.filter_field.read(cx).text().to_owned())
    };
    assert_eq!(query(&mut s), "role=reviewer");
    assert_eq!(tabs_drawn(&mut s), [1], "filtered out");
    // The chip's click is not the row's: no pane shows the agent.
    let pane = focused(&mut s);
    assert_eq!(shown(&mut s, pane), None);
    click(&mut s, "tab-1-chip-role");
    assert_eq!(query(&mut s), "");
    let mut all = tabs_drawn(&mut s);
    all.sort();
    assert_eq!(all, [1, 2]);
}
