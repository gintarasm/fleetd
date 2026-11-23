use super::*;

#[gpui::test]
fn new_agent_starts_in_the_default_folder(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = GuiState::default();
    state.recent_dirs.insert("local".into(), vec!["/r1".into()]);
    state.save(dir.path()).unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        default_dir: "/projects".into(),
        ..GuiConfig::default()
    };
    let s = setup_with(cx, &fake, dir.path(), config);
    s.cx.simulate_keystrokes("cmd-n");
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.sheet().cloned())
            .expect("sheet open");
    let (dir_text, dirs) = sheet.read_with(s.cx, |sh, cx| {
        (sh.dir.read(cx).text().to_owned(), sh.dirs.clone())
    });
    assert_eq!(dir_text, "/projects");
    // The recent ones are still offered.
    assert_eq!(dirs, ["/r1"]);
}

#[gpui::test]
fn new_agent_prefills_default_command(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = GuiState::default();
    state
        .recent_dirs
        .insert("local".into(), vec!["/r1".into(), "/a".into()]);
    state.save(dir.path()).unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        default_command: vec![
            "claude".into(),
            "--append-system-prompt".into(),
            "be brief".into(),
        ],
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    agents(
        &mut s,
        &fake,
        vec![row(1, "/a", 1), row(2, "/b", 3), row(3, "/c", 2)],
    );
    s.cx.simulate_keystrokes("cmd-n");
    let sheet =
        s.ws.read_with(s.cx, |w, _| w.sheet().cloned())
            .expect("sheet open");
    let (command, dirs, dir_text, can_choose) = sheet.read_with(s.cx, |sh, cx| {
        (
            sh.command.read(cx).text().to_owned(),
            sh.dirs.clone(),
            sh.dir.read(cx).text().to_owned(),
            sh.can_choose,
        )
    });
    // Shell-quoted, so the argument with a space survives the parse.
    assert_eq!(command, "claude --append-system-prompt 'be brief'");
    assert_eq!(dirs, ["/r1", "/a", "/b", "/c"]);
    assert_eq!(dir_text, "/r1");
    assert!(can_choose);

    let name = sheet.read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
    assert!(name.starts_with("brief-"), "{name}");
    s.cx.simulate_keystrokes("enter");
    assert!(fake.calls().contains(&Call::Spawn {
        req: SpawnRequest {
            program: "claude".into(),
            args: vec!["--append-system-prompt".into(), "be brief".into()],
            cwd: "/r1".into(),
            labels: HashMap::from([("name".into(), name)]),
        },
        size: s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
        }),
        pane: PaneId(1),
    }));

    // Again, typing a command with quoting into the command field.
    s.cx.simulate_keystrokes("cmd-n");
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let field = sheet.read_with(s.cx, |sh, _| sh.command.clone());
    field.update(s.cx, |f, cx| f.set_text(String::new(), cx));
    s.cx.simulate_input("sh -c 'echo hi'");
    let dir_field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
    dir_field.update(s.cx, |f, cx| f.set_text("/tmp".into(), cx));
    s.cx.run_until_parked();
    let name = sheet.read_with(s.cx, |sh, cx| sh.name.read(cx).text().to_owned());
    assert!(name.starts_with("echo-"), "{name}");
    s.cx.simulate_keystrokes("enter");

    let size = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).grid().unwrap()
    });
    assert!(fake.calls().contains(&Call::Spawn {
        req: SpawnRequest {
            program: "sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            cwd: "/tmp".into(),
            labels: HashMap::from([("name".into(), name)]),
        },
        size,
        pane: PaneId(1),
    }));
    assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));
    let recent = |s: &mut Setup<'_>| {
        s.app
            .read_with(s.cx, |a, _| a.gui_state().recent_dirs["local"].clone())
    };
    // Not yet: the spawn has not landed.
    assert_eq!(recent(&mut s), ["/r1", "/a"]);

    // The spawned agent lands in the pane: the layout records it.
    fake.model(PaneId(1))
        .lock()
        .unwrap()
        .on_attached(&AttachOk {
            agent_id: 9,
            cols: 80,
            rows: 24,
            primary: true,
            ..Default::default()
        });
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    let shown = s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
    assert_eq!(shown, Some(9));
    assert_eq!(recent(&mut s), ["/tmp", "/r1", "/a"]);
}

/// ⌘T: a login shell labelled `kind=shell`, in the last-used folder
/// for an empty pane, then in the focused agent's folder, in a new
/// split that takes the focus.
#[gpui::test]
fn new_terminal_spawns_a_shell_and_focuses_it(cx: &mut TestAppContext) {
    use crate::core::spawn::{KIND_LABEL, SHELL_KIND};
    let dir = tempfile::tempdir().unwrap();
    let mut state = GuiState::default();
    state.recent_dirs.insert("local".into(), vec!["/r1".into()]);
    state.save(dir.path()).unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup_with(cx, &fake, dir.path(), GuiConfig::default());
    let spawns = |fake: &Fake| -> Vec<(SpawnRequest, PaneId)> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Spawn { req, pane, .. } => Some((req, pane)),
                _ => None,
            })
            .collect()
    };

    s.cx.simulate_keystrokes("cmd-t");
    let got = spawns(&fake);
    assert_eq!(got.len(), 1);
    let (req, pane) = &got[0];
    assert_eq!((req.cwd.as_str(), *pane), ("/r1", PaneId(1)));
    assert_eq!(req.args, ["-l"]);
    assert_eq!(req.labels[KIND_LABEL], SHELL_KIND);
    assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_none()));

    // Pane 1 now shows an agent in /a: the next one splits.
    agents(&mut s, &fake, vec![row(9, "/a", 1)]);
    attach_model(&fake, PaneId(1), 9);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s.cx.simulate_keystrokes("cmd-t");
    let got = spawns(&fake);
    assert_eq!(got.len(), 2);
    let (req, pane) = &got[1];
    assert_eq!((req.cwd.as_str(), *pane), ("/a", PaneId(2)));
    let (focused, shown) = s
        .app
        .read_with(s.cx, |a, _| (a.focused(), a.layout().agent(PaneId(1))));
    assert_eq!((focused, shown), (PaneId(2), Some(9)));
}

/// The Name field follows the command and directory until the user
/// edits it; the generated name avoids live agents' names; labels
/// are `k=v` pairs.
#[gpui::test]
fn new_agent_name_follows_the_command_until_edited(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-n");
    let name = |sh: &NewAgentSheet| sh.name.clone();
    let command = |sh: &NewAgentSheet| sh.command.clone();
    set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
    set_sheet_text(&mut s, command, "claude");
    assert!(sheet_text(&mut s, name).starts_with("fleetd-"));
    set_sheet_text(&mut s, command, "claude 'fix the SIGTERM race'");
    let generated = sheet_text(&mut s, name);
    assert!(generated.starts_with("sigterm-"), "{generated}");
    // Typed by the user: it stays.
    set_sheet_text(&mut s, name, "my-agent");
    set_sheet_text(&mut s, command, "claude 'tune LTO'");
    assert_eq!(sheet_text(&mut s, name), "my-agent");
    set_sheet_text(&mut s, |sh| sh.labels.clone(), "role=reviewer, team=core");
    s.cx.simulate_keystrokes("enter");
    let req = last_spawn(&fake);
    assert_eq!(req.args, ["tune LTO"]);
    assert_eq!(
        req.labels,
        HashMap::from([
            ("name".to_owned(), "my-agent".to_owned()),
            ("role".to_owned(), "reviewer".to_owned()),
            ("team".to_owned(), "core".to_owned()),
        ])
    );

    // A live agent has the name the command would get: another one.
    let mut taken = row(1, "/w/fleetd", 1);
    taken.info.labels.insert("name".into(), generated.clone());
    agents(&mut s, &fake, vec![taken]);
    s.cx.simulate_keystrokes("cmd-n");
    set_sheet_text(&mut s, |sh| sh.dir.clone(), "/w/fleetd");
    set_sheet_text(&mut s, command, "claude 'fix the SIGTERM race'");
    let other = sheet_text(&mut s, name);
    assert!(
        other.starts_with("sigterm-") && other != generated,
        "{other}"
    );
    // Bad labels or names keep the sheet open with the error.
    set_sheet_text(&mut s, |sh| sh.labels.clone(), "role");
    s.cx.simulate_keystrokes("enter");
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    assert!(sheet.read_with(s.cx, |sh, _| sh.error.clone()).is_some());
    set_sheet_text(&mut s, |sh| sh.labels.clone(), "");
    set_sheet_text(&mut s, name, "12");
    s.cx.simulate_keystrokes("enter");
    assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_some()));
}
