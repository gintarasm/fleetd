use super::*;

fn groups_drawn(s: &mut Setup<'_>) -> Vec<String> {
    s.cx.run_until_parked();
    s.ws.read_with(s.cx, |w, _| {
        w.drawn_sidebar.iter().map(|(t, _)| t.clone()).collect()
    })
}

fn filter_text(s: &mut Setup<'_>) -> String {
    s.ws.read_with(s.cx, |w, cx| w.filter_field.read(cx).text().to_owned())
}

fn filter_focused(s: &mut Setup<'_>) -> bool {
    s.ws.update_in(s.cx, |w, window, cx| {
        w.filter_field.read(cx).focus_handle().is_focused(window)
    })
}

fn pane_focused(s: &mut Setup<'_>) -> bool {
    let pane = focused(s);
    s.ws.update_in(s.cx, |w, window, cx| {
        w.pane_focus_handle(pane, cx)
            .is_some_and(|h| h.is_focused(window))
    })
}

#[gpui::test]
fn cmd_shift_f_filters_by_name_cwd_and_labels_and_escape_clears(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut coder = project(row(2, "/srv/beta", 2), "beta");
    coder.info.labels.insert("role".into(), "coder".into());
    agents(
        &mut s,
        &fake,
        vec![project(row(1, "/srv/alpha", 1), "alpha"), coder],
    );
    assert_eq!(tabs_drawn(&mut s), [1, 2]);

    s.cx.simulate_keystrokes("cmd-alt-f");
    assert!(filter_focused(&mut s));
    s.cx.simulate_input("coder");
    assert_eq!(tabs_drawn(&mut s), [2]);
    assert!(drawn(&mut s, "tab-2"));
    assert_eq!(groups_drawn(&mut s), ["beta"]);

    s.ws.update(s.cx, |w, cx| {
        w.filter_field
            .update(cx, |f, cx| f.set_text("srv/alpha".into(), cx))
    });
    assert_eq!(tabs_drawn(&mut s), [1]);

    s.ws.update(s.cx, |w, cx| {
        w.filter_field
            .update(cx, |f, cx| f.set_text("nothing".into(), cx))
    });
    assert!(tabs_drawn(&mut s).is_empty());
    assert!(drawn(&mut s, "sidebar-no-match"));

    // Escape clears the text, then a second one gives the pane the keys.
    s.cx.simulate_keystrokes("escape");
    assert_eq!(filter_text(&mut s), "");
    assert_eq!(tabs_drawn(&mut s), [1, 2]);
    assert!(filter_focused(&mut s));
    s.cx.simulate_keystrokes("escape");
    assert!(pane_focused(&mut s));
}

#[gpui::test]
fn enter_in_the_filter_shows_the_first_match(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
    s.cx.simulate_keystrokes("cmd-alt-f");
    s.cx.simulate_input("/b");
    s.cx.simulate_keystrokes("enter");
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::Attach(PaneId(1), 2)));
    assert!(pane_focused(&mut s));
}

#[gpui::test]
fn typing_in_the_sidebar_goes_to_the_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/a", 1), row(2, "/b", 2)]);
    // A click on the sidebar's empty space below the tabs.
    let tabs = s.cx.debug_bounds("sidebar").expect("sidebar drawn");
    let at = point(tabs.left() + gpui::px(20.), tabs.bottom() - gpui::px(10.));
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert!(!pane_focused(&mut s));
    s.cx.simulate_keystrokes("b");
    assert!(filter_focused(&mut s));
    assert_eq!(filter_text(&mut s), "b");
    assert_eq!(tabs_drawn(&mut s), [2]);
}

#[gpui::test]
fn status_filters_narrow_the_sidebar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut quiet = row(3, "/c", 3);
    quiet.last_line = "Continue? (y/n)".into();
    quiet.last_output_unix_ms = Some(1);
    agents(
        &mut s,
        &fake,
        vec![
            row(1, "/a", 1),
            ended(2, "/b", 2, AgentState::Exited),
            quiet,
        ],
    );
    let tabs = |s: &mut Setup<'_>| {
        let mut t = tabs_drawn(s);
        t.sort_unstable();
        t
    };
    assert_eq!(tabs(&mut s), [1, 2, 3]);
    click(&mut s, "status-filter-Exited");
    assert_eq!(tabs(&mut s), [2]);
    click(&mut s, "status-filter-Running");
    assert_eq!(tabs(&mut s), [1, 3]);
    click(&mut s, "status-filter-Needs you");
    assert_eq!(tabs(&mut s), [3]);
    click(&mut s, "status-filter-All");
    assert_eq!(tabs(&mut s), [1, 2, 3]);
}

#[gpui::test]
fn a_group_collapses_with_a_count_and_is_remembered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            project(row(1, "/a", 1), "alpha"),
            project(row(2, "/a", 2), "alpha"),
            project(row(3, "/b", 3), "beta"),
        ],
    );
    assert!(!drawn(&mut s, "group-count-alpha"));
    click(&mut s, "group-alpha");
    assert_eq!(tabs_drawn(&mut s), [3]);
    assert_eq!(groups_drawn(&mut s), ["alpha", "beta"]);
    assert!(drawn(&mut s, "group-count-alpha"));
    let count = s.cx.debug_bounds("group-count-alpha-2");
    assert!(count.is_some(), "the count says 2");

    s.cx.executor().advance_clock(SAVE_DEBOUNCE);
    s.cx.run_until_parked();
    let saved = GuiState::load(dir.path()).collapsed_groups;
    assert_eq!(saved["local"].iter().collect::<Vec<_>>(), ["alpha"]);

    // Typing shows what matched inside it, without forgetting it.
    s.cx.simulate_keystrokes("cmd-alt-f");
    s.cx.simulate_input("/a");
    assert_eq!(tabs_drawn(&mut s), [2, 1]);
    s.cx.simulate_keystrokes("escape");
    assert_eq!(tabs_drawn(&mut s), [3]);

    click(&mut s, "group-alpha");
    assert_eq!(tabs_drawn(&mut s), [2, 1, 3]);
    assert!(
        s.app
            .read_with(s.cx, |a, _| a.collapsed_groups().is_empty())
    );
}

fn in_repo(mut r: AgentRow, repo: &str, worktree: &str, branch: &str) -> AgentRow {
    r.info.git = Some(fleet_proto::GitInfo {
        repo: repo.into(),
        worktree: worktree.into(),
        branch: branch.into(),
        detached: false,
    });
    r
}

/// ADR 0085: the row and the pane header show the git checkout; the
/// sidebar groups by repository on request, remembered; the filter
/// matches the branch.
#[gpui::test]
fn rows_show_the_branch_and_group_by_repository(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            in_repo(row(1, "/src/fleetd", 1), "fleetd", "", "main"),
            in_repo(row(2, "/wt/issue-97", 2), "fleetd", "issue-97", "feat/git"),
            row(3, "/notes", 3),
        ],
    );
    assert!(drawn(&mut s, "tab-1-git-fleetd · main"));
    assert!(drawn(&mut s, "tab-2-git-issue-97 · feat/git"));
    // Not in a checkout: program and id, as before.
    assert!(drawn(&mut s, "tab-3-detail"));
    assert_eq!(
        groups_drawn(&mut s),
        ["/notes", "/src/fleetd", "/wt/issue-97"]
    );

    s.app.update(s.cx, |a, cx| a.show_agent(2, cx));
    assert!(drawn(&mut s, "pane-1-git-fleetd (issue-97) · feat/git"));

    s.ws.update(s.cx, |ws, cx| {
        ws.set_group_by(crate::core::sidebar::GroupBy::Repo, cx)
    });
    assert_eq!(groups_drawn(&mut s), ["/notes", "fleetd"]);
    assert_eq!(tabs_drawn(&mut s), [3, 2, 1]);
    s.cx.executor().advance_clock(SAVE_DEBOUNCE);
    s.cx.run_until_parked();
    assert_eq!(
        GuiState::load(dir.path()).sidebar_group_by,
        crate::core::sidebar::GroupBy::Repo
    );

    s.cx.simulate_keystrokes("cmd-alt-f");
    s.cx.simulate_input("feat/git");
    assert_eq!(tabs_drawn(&mut s), [2]);
}

/// #147: the sidebar borrows the app state rather than a per-frame copy,
/// so every title, status, bell and tab-mark change reaches the next
/// frame. Fails if any of them is read from a snapshot that is not
/// refreshed when it changes.
#[gpui::test]
fn each_frame_draws_the_current_titles_statuses_bells_and_marks(cx: &mut TestAppContext) {
    use super::super::DrawnRow;
    use crate::core::status::AgentStatus;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
    s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
    fake.model(PaneId(1))
        .lock()
        .unwrap()
        .on_attached(&AttachOk {
            agent_id: 1,
            cols: 80,
            rows: 24,
            ..Default::default()
        });
    s.cx.run_until_parked();
    let drawn = |s: &mut Setup<'_>, id: u64| -> DrawnRow {
        s.cx.run_until_parked();
        s.ws.read_with(s.cx, |w, _| {
            w.drawn_rows.iter().find(|r| r.id == id).cloned().unwrap()
        })
    };
    let output = |s: &mut Setup<'_>, bytes: &[u8]| {
        fake.model(PaneId(1)).lock().unwrap().on_output(bytes);
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
    };
    let before = drawn(&mut s, 1);
    assert_eq!(before.mark, TabMark::Focused);
    assert_eq!(before.status, Some(AgentStatus::Idle));
    assert!(!before.bell);
    assert_eq!(drawn(&mut s, 2).mark, TabMark::None);

    // A title set after a frame shows on the next one, and so does its
    // replacement.
    output(&mut s, b"\x1b]2;building\x07");
    assert_eq!(drawn(&mut s, 1).title, "building");
    output(&mut s, b"\x1b]2;testing\x07");
    assert_eq!(drawn(&mut s, 1).title, "testing");

    // A bell away from the window marks the row and its status; seeing
    // the pane clears both.
    s.app.update(s.cx, |a, cx| a.set_window_active(false, cx));
    output(&mut s, b"ding\x07");
    let rang = drawn(&mut s, 1);
    assert!(rang.bell);
    assert_eq!(rang.status, Some(AgentStatus::NeedsYou));
    s.app.update(s.cx, |a, cx| a.set_window_active(true, cx));
    let seen = drawn(&mut s, 1);
    assert!(!seen.bell);
    assert_eq!(seen.status, Some(AgentStatus::Idle));

    // Showing the other agent moves the tab marks.
    s.app.update(s.cx, |a, cx| a.show_agent(2, cx));
    assert_eq!(drawn(&mut s, 2).mark, TabMark::Focused);
    assert_eq!(drawn(&mut s, 1).mark, TabMark::None);

    // An AgentsChanged rename (the name wins over the title) reaches the
    // rows too.
    let mut renamed = row(1, "/w", 1);
    renamed
        .info
        .labels
        .insert(fleet_client::names::NAME_LABEL.into(), "renamed".into());
    agents(&mut s, &fake, vec![renamed, row(2, "/w", 2)]);
    assert_eq!(drawn(&mut s, 1).title, "renamed");
    assert_eq!(
        s.ws.read_with(s.cx, |w, _| w.drawn_rows.len()),
        2,
        "one drawn row per agent"
    );
}
