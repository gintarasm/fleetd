use super::*;

/// The Search All Agents rows: (heading, title).
fn search_rows(s: &mut Setup<'_>) -> Vec<(Option<String>, String)> {
    s.ws.read_with(s.cx, |w, cx| {
        let view = w.search_all().expect("search open").read(cx);
        let picker = view.picker().read(cx);
        let titles: Vec<String> = picker
            .picker()
            .items()
            .iter()
            .map(|i| i.title.clone())
            .collect();
        picker.sections().iter().cloned().zip(titles).collect()
    })
}

#[gpui::test]
fn search_all_agents_lists_hits_by_agent_and_finds_the_pick(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
    let hit = |agent_id, line, text: &str| fleet_proto::SearchHit {
        agent_id,
        line,
        text: text.into(),
    };
    *fake.search_hits.lock().unwrap() = vec![
        hit(2, 7, "  error: two"),
        hit(1, 3, "error in one"),
        hit(1, 1, "more error"),
    ];
    s.cx.simulate_keystrokes("cmd-shift-f");
    assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_some()));
    s.cx.simulate_input("err");
    s.cx.run_until_parked();
    assert!(fake.searches.lock().unwrap().is_empty(), "debounced");
    s.cx.simulate_input("or");
    s.cx.executor()
        .advance_clock(crate::core::search_all::DEBOUNCE);
    s.cx.run_until_parked();
    assert_eq!(
        *fake.searches.lock().unwrap(),
        [("error".to_owned(), false)],
        "one search, for the latest query"
    );
    let rows = search_rows(&mut s);
    let titles: Vec<&str> = rows.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(titles, ["error: two", "error in one", "more error"]);
    let headed: Vec<bool> = rows.iter().map(|(h, _)| h.is_some()).collect();
    assert_eq!(headed, [true, true, false], "grouped by agent");

    // The second row: agent 1 in the focused pane, find on line 3.
    s.cx.simulate_keystrokes("down enter");
    s.cx.run_until_parked();
    assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_none()));
    let pane = focused(&mut s);
    assert_eq!(
        s.app.read_with(s.cx, |a, _| a.layout().agent(pane)),
        Some(1)
    );
    let lines: Vec<bytes::Bytes> = ["error 0", "error 1", "x", "error 3", "error 4"]
        .iter()
        .map(|l| bytes::Bytes::from(l.as_bytes().to_vec()))
        .collect();
    fake.model(pane).lock().unwrap().on_attached(&AttachOk {
        agent_id: 1,
        cols: 80,
        rows: 24,
        primary: true,
        history_tail: lines,
        ..Default::default()
    });
    let view = s
        .app
        .read_with(s.cx, |a, _| a.pane_view(pane).cloned())
        .unwrap();
    // The model changed under the view: a redraw scans it.
    view.update(s.cx, |_, cx| cx.notify());
    s.cx.run_until_parked();
    let label = view.read_with(s.cx, |v, _| v.find_label());
    assert_eq!(label.as_deref(), Some("3 of 4"), "on line 3");
}

#[gpui::test]
fn filter_agents_moved_to_option_command_f(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let rows = palette_rows(&mut s);
    assert!(rows.contains(&("Search All Agents…".to_owned(), Some("⇧⌘F".to_owned()))));
    assert!(rows.contains(&("Filter Agents".to_owned(), Some("⌥⌘F".to_owned()))));
    s.cx.simulate_keystrokes("cmd-shift-f escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.search_all().is_none()));
}

/// The palette's rows now, as (title, shortcut); closes it again.
fn palette_rows(s: &mut Setup<'_>) -> Vec<(String, Option<String>)> {
    s.cx.simulate_keystrokes("cmd-p");
    let rows = s.ws.read_with(s.cx, |w, cx| {
        let palette = w.palette().expect("palette open").read(cx);
        palette
            .picker()
            .items()
            .iter()
            .map(|i| (i.title.clone(), i.shortcut.clone()))
            .collect()
    });
    s.cx.simulate_keystrokes("escape");
    rows
}

fn has_row(rows: &[(String, Option<String>)], title: &str) -> bool {
    rows.iter().any(|(t, _)| t == title)
}

fn pick_in_palette(s: &mut Setup<'_>, query: &str) {
    s.cx.simulate_keystrokes("cmd-p");
    s.cx.simulate_input(query);
    s.cx.simulate_keystrokes("enter");
    s.cx.run_until_parked();
}

/// What acts on the focused pane's agent shows only while it has one,
/// in the right state.
#[gpui::test]
fn agent_commands_follow_the_focused_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    let rows = palette_rows(&mut s);
    for title in [
        "Kill Agent…",
        "Rename Agent…",
        "Edit Labels…",
        "Find in Scrollback…",
        "Restart Agent in Pane",
        "Remove Exited Agent in Pane",
    ] {
        assert!(!has_row(&rows, title), "{title} with an empty pane");
    }
    assert!(has_row(&rows, "Zoom In") && has_row(&rows, "Settings…"));

    click(&mut s, "tab-7");
    let rows = palette_rows(&mut s);
    assert!(has_row(&rows, "Kill Agent…") && has_row(&rows, "Rename Agent…"));
    assert!(has_row(&rows, "Edit Labels…"));
    assert!(!has_row(&rows, "Restart Agent in Pane"));
    pick_in_palette(&mut s, "kill agent");
    assert_eq!(kill_asked(&mut s), Some(7));
    s.cx.simulate_keystrokes("escape");
}

#[gpui::test]
fn rename_and_remove_from_the_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![named(3, "old-name", &[])]);
    click(&mut s, "tab-3");
    pick_in_palette(&mut s, "rename agent");
    rename_to(&mut s, "from-palette");
    assert!(
        fake.calls()
            .contains(&Call::Rename(3, "from-palette".into()))
    );
    pick_in_palette(&mut s, "edit labels");
    assert!(s.ws.read_with(s.cx, |w, _| w.labels_sheet().is_some()));
    s.cx.simulate_keystrokes("escape");

    agents(&mut s, &fake, vec![ended(3, "/w", 3, AgentState::Exited)]);
    attach_model(&fake, PaneId(1), 3);
    fake.model(PaneId(1)).lock().unwrap().on_exited(3);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    let rows = palette_rows(&mut s);
    assert!(has_row(&rows, "Remove Exited Agent in Pane"));
    assert!(!has_row(&rows, "Kill Agent…"));
    pick_in_palette(&mut s, "remove exited agent in pane");
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![3])));
}

fn presets_config() -> GuiConfig {
    let preset = |name: &str, command: &[&str], labels: &[(&str, &str)]| Preset {
        name: name.into(),
        command: command.iter().map(|w| (*w).to_owned()).collect(),
        labels: labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    GuiConfig {
        presets: Some(vec![
            preset("Claude", &["claude"], &[]),
            preset(
                "Review",
                &["claude", "--model", "opus"],
                &[("role", "reviewer")],
            ),
        ]),
        ..GuiConfig::default()
    }
}

/// A preset's button fills the command and labels; the prompt is the
/// last argument and names the agent; the command becomes recent once
/// the pane shows the agent.
#[gpui::test]
fn sheet_presets_prompt_and_recent_commands(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup_with(cx, &fake, dir.path(), presets_config());
    s.cx.simulate_keystrokes("cmd-n");
    set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
    click(&mut s, "preset-1");
    assert_eq!(
        sheet_text(&mut s, |sh| sh.command.clone()),
        "claude --model opus"
    );
    assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=reviewer");
    // The caret is in Prompt: typing goes there.
    s.cx.simulate_input("fix the SIGTERM race");
    assert_eq!(
        sheet_text(&mut s, |sh| sh.prompt.clone()),
        "fix the SIGTERM race"
    );
    let name = sheet_text(&mut s, |sh| sh.name.clone());
    assert!(name.starts_with("sigterm-"), "{name}");
    s.cx.simulate_keystrokes("enter");
    let req = last_spawn(&fake);
    assert_eq!(req.program, "claude");
    assert_eq!(req.args, ["--model", "opus", "fix the SIGTERM race"]);
    assert_eq!(req.labels["role"], "reviewer");
    assert_eq!(req.labels["name"], name);
    // Not recent until the pane shows the agent.
    let recent = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.recent_commands());
    assert!(recent(&mut s).is_empty());
    attach_model(&fake, PaneId(1), 9);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(recent(&mut s), ["claude --model opus"]);
    // The next sheet offers it.
    s.cx.simulate_keystrokes("cmd-n");
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let commands = sheet.read_with(s.cx, |sh, _| sh.commands.clone());
    assert_eq!(commands, ["claude --model opus"]);
}

/// Duplicate in the row and pane menus and the palette: the sheet with
/// the agent's command, folder and labels, and a new name.
#[gpui::test]
fn duplicate_prefills_the_sheet_from_a_running_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut agent = named(3, "build-otter", &[("role", "fixer")]);
    agent.info.program = "claude".into();
    agent.info.args = vec!["fix the build".into()];
    agents(&mut s, &fake, vec![agent]);
    assert!(row_items(&mut s, 3).contains(&RowMenuItem::Duplicate));
    pick_row(&mut s, 3, RowMenuItem::Duplicate);
    assert_eq!(
        sheet_text(&mut s, |sh| sh.command.clone()),
        "claude 'fix the build'"
    );
    assert_eq!(sheet_text(&mut s, |sh| sh.dir.clone()), "/w");
    assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=fixer");
    let name = sheet_text(&mut s, |sh| sh.name.clone());
    assert!(!name.is_empty() && name != "build-otter", "{name}");
    s.cx.simulate_keystrokes("escape");
    assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));

    // From the pane's menu, and the palette, on the pane's agent.
    click(&mut s, "tab-3");
    let epoch = s.ws.read_with(s.cx, |ws, _| ws.epoch);
    s.ws.update_in(s.cx, |ws, window, cx| {
        ws.pane_menu_pick(PaneId(1), PaneMenuItem::Duplicate, epoch, window, cx);
    });
    s.cx.run_until_parked();
    assert_eq!(sheet_text(&mut s, |sh| sh.labels.clone()), "role=fixer");
    s.cx.simulate_keystrokes("escape");
    pick_in_palette(&mut s, "duplicate agent");
    assert_eq!(
        sheet_text(&mut s, |sh| sh.command.clone()),
        "claude 'fix the build'"
    );
}

/// Presets in the palette open the sheet prefilled; in the split
/// chooser they start at once in its folder.
#[gpui::test]
fn presets_in_the_palette_and_the_split_chooser(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup_with(cx, &fake, dir.path(), presets_config());
    let rows = palette_rows(&mut s);
    assert!(has_row(&rows, "New Claude…") && has_row(&rows, "New Review…"));
    pick_in_palette(&mut s, "new review");
    assert_eq!(
        sheet_text(&mut s, |sh| sh.command.clone()),
        "claude --model opus"
    );
    s.cx.simulate_keystrokes("escape");

    agents(&mut s, &fake, vec![row(7, "/w/fleetd", 1)]);
    click(&mut s, "tab-7");
    s.cx.simulate_keystrokes("cmd-d");
    let titles = s.ws.read_with(s.cx, |w, cx| {
        let chooser = w.chooser(PaneId(2)).expect("chooser").read(cx);
        chooser
            .chooser()
            .rows()
            .iter()
            .map(|r| r.title.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(titles[3..], ["Claude", "Review"]);
    s.cx.simulate_keystrokes("5");
    s.cx.run_until_parked();
    let req = last_spawn(&fake);
    assert_eq!(req.cwd, "/w/fleetd");
    assert_eq!(req.args, ["--model", "opus"]);
    assert_eq!(req.labels["role"], "reviewer");
    assert!(req.labels.contains_key("name"));
}

/// Tab commands show with more than one tab, each with its shortcut.
#[gpui::test]
fn tab_commands_show_their_shortcuts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let rows = palette_rows(&mut s);
    assert!(has_row(&rows, "New Tab"));
    assert!(!has_row(&rows, "Show Next Tab") && !has_row(&rows, "Go to Tab 1"));
    s.cx.simulate_keystrokes("cmd-alt-t");
    let rows = palette_rows(&mut s);
    let shortcut = |title: &str| {
        rows.iter()
            .find(|(t, _)| t == title)
            .unwrap_or_else(|| panic!("{title} listed"))
            .1
            .clone()
    };
    assert_eq!(shortcut("Go to Tab 2").as_deref(), Some("⌥⌘2"));
    assert_eq!(shortcut("Show Next Tab").as_deref(), Some("⇧⌘]"));
    assert_eq!(shortcut("Close Tab").as_deref(), Some("⌥⌘W"));
    assert!(!has_row(&rows, "Go to Tab 3"));
    pick_in_palette(&mut s, "go to tab 1");
    assert_eq!(s.app.read_with(s.cx, |a, _| a.tabs().active()), 0);
}

#[gpui::test]
fn clear_exited_disabled_without_exited_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    click(&mut s, "clear-exited");
    assert!(!fake.calls().contains(&Call::ClearExited));
    agents(
        &mut s,
        &fake,
        vec![row(7, "/w", 1), ended(8, "/w", 2, AgentState::Exited)],
    );
    click(&mut s, "clear-exited");
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
}
