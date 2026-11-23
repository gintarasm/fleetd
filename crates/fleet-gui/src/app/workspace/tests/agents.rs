use super::*;

#[gpui::test]
fn spawn_is_adopted_though_another_spawn_failed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    spawn_via_sheet(&mut s, "/one");
    s.cx.simulate_keystrokes("cmd-d");
    spawn_via_sheet(&mut s, "/two");
    // Pane 1's spawn fails; the error is not pane-specific.
    s.ui.try_send(UiEvent::Error(
        "spawn failed: /one: no such directory".into(),
    ))
    .unwrap();
    s.cx.run_until_parked();
    // Pane 2's spawn lands afterwards.
    attach_model(&fake, PaneId(2), 9);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(2))).unwrap();
    s.cx.run_until_parked();
    let (one, two, recent) = s.app.read_with(s.cx, |a, _| {
        (
            a.layout().agent(PaneId(1)),
            a.layout().agent(PaneId(2)),
            a.gui_state()
                .recent_dirs
                .get("local")
                .cloned()
                .unwrap_or_default(),
        )
    });
    assert_eq!((one, two), (None, Some(9)));
    assert_eq!(recent, ["/two"]);
    let chrome = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(2)).unwrap().read(cx).chrome.clone()
    });
    assert!(chrome.has_agent && !chrome.starting);
}

#[gpui::test]
fn file_links_resolve_in_a_local_agents_directory_only(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    s.cx.run_until_parked();
    let chrome = |s: &mut Setup<'_>| {
        s.app.read_with(s.cx, |a, cx| {
            let c = a.pane_view(PaneId(1)).unwrap().read(cx).chrome.clone();
            (c.cwd, c.link_dir)
        })
    };
    assert_eq!(chrome(&mut s), (Some("/w".into()), Some("/w".into())));

    // On another machine the agent's paths are not here.
    let other = Arc::new(Fake::default());
    let session: Arc<dyn SessionApi> = other.clone();
    let (_tx, rx) = async_channel::unbounded();
    let target = Target::Ssh {
        dest: "me@box".into(),
    };
    *other.agents.lock().unwrap() = vec![row(5, "/w", 1)];
    s.app
        .update(s.cx, |a, cx| a.replace_session(session, rx, target, cx));
    click(&mut s, "tab-5");
    attach_model(&other, PaneId(1), 5);
    s.cx.run_until_parked();
    assert_eq!(chrome(&mut s), (Some("/w".into()), None));
}

#[gpui::test]
fn requested_attach_is_not_reverted_by_the_old_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(5, "/w", 1), row(7, "/w", 2)]);
    click(&mut s, "tab-5");
    attach_model(&fake, PaneId(1), 5);
    click(&mut s, "tab-7");
    // Output from agent 5 still arrives before the attach reply.
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    let shown = |s: &mut Setup<'_>| s.app.read_with(s.cx, |a, _| a.layout().agent(PaneId(1)));
    assert_eq!(shown(&mut s), Some(7));
    attach_model(&fake, PaneId(1), 7);
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    assert_eq!(shown(&mut s), Some(7));
}

#[gpui::test]
fn header_buttons_work_after_closing_the_last_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.simulate_keystrokes("cmd-w");
    assert_eq!(panes(&mut s), [PaneId(1)]);
    assert!(fake.calls().contains(&Call::Close(PaneId(1))));
    click(&mut s, "pane-1-split-right");
    assert_eq!(panes(&mut s).len(), 2);
    // Click-to-focus still follows the new view.
    let grid = s.app.read_with(s.cx, |a, cx| {
        a.pane_view(PaneId(1)).unwrap().read(cx).layout.unwrap()
    });
    let at = point(
        grid.origin.x + grid.cell_w * 2.0,
        grid.origin.y + grid.line_h * 2.0,
    );
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(focused(&mut s), PaneId(1));
}

#[gpui::test]
fn failed_spawn_is_shown(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let s = setup(cx, &fake, dir.path());
    *fake.spawn_error.lock().unwrap() = Some("no pane 1".into());
    s.cx.simulate_keystrokes("cmd-n");
    let sheet = s.ws.read_with(s.cx, |w, _| w.sheet().cloned()).unwrap();
    let dir_field = sheet.read_with(s.cx, |sh, _| sh.dir.clone());
    dir_field.update(s.cx, |f, cx| f.set_text("/tmp".into(), cx));
    s.cx.simulate_keystrokes("enter");
    // The sheet stays open with the error.
    let error = sheet.read_with(s.cx, |sh, _| sh.error.clone());
    assert_eq!(error.as_deref(), Some("no pane 1"));
    assert!(s.ws.read_with(s.cx, |w, _| w.sheet().is_some()));

    // A failure reported later by the session shows as the error toast.
    s.ui.try_send(UiEvent::Error("spawn failed: no such directory".into()))
        .unwrap();
    s.cx.run_until_parked();
    let shown = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
    assert_eq!(shown.as_deref(), Some("spawn failed: no such directory"));
    assert!(s.cx.debug_bounds("error-toast").is_some());
}

/// Issue #41: a row shows the agent's last line under its name, and
/// its run time, or how it ended, on the right.
#[gpui::test]
fn rows_show_last_line_and_run_time_or_exit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut running = row(1, "/w", now_ms() - 125_000);
    running.last_line = "compiling\tfleetd\n".into();
    agents(
        &mut s,
        &fake,
        vec![
            running,
            ended(2, "/w", 2, AgentState::Exited),
            ended(3, "/w", 3, AgentState::Lost),
        ],
    );
    assert!(s.cx.debug_bounds("meta-1-2m").is_some());
    assert!(s.cx.debug_bounds("preview-1-compiling fleetd").is_some());
    assert!(s.cx.debug_bounds("meta-2-exit 3").is_some());
    assert!(s.cx.debug_bounds("meta-3-lost").is_some());
}

/// Issue #41: output alone redraws a row at most once a second (on the
/// sidebar's tick); a change of state redraws at once.
#[gpui::test]
fn output_alone_waits_for_the_tick(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1)]);
    let notified = Arc::new(AtomicUsize::new(0));
    let n = notified.clone();
    let _sub = s.cx.update(|_, cx| {
        cx.observe(&s.app, move |_, _| {
            n.fetch_add(1, Ordering::SeqCst);
        })
    });

    let mut printed = row(1, "/w", 1);
    printed.last_line = "new output".into();
    // Quiet output: the status stays idle (a new status redraws at once).
    printed.last_output_unix_ms = Some(now_ms() - 60_000);
    agents(&mut s, &fake, vec![printed.clone()]);
    assert_eq!(notified.load(Ordering::SeqCst), 0);
    // The data is current (statuses read it); only the redraw waits.
    let line = s.app.read_with(s.cx, |a, _| a.agents[0].last_line.clone());
    assert_eq!(line, "new output");
    s.cx.executor().advance_clock(Duration::from_secs(1));
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("preview-1-new output").is_some());

    let mut exited = printed;
    exited.info.set_state(AgentState::Exited);
    exited.info.exit_code = Some(0);
    agents(&mut s, &fake, vec![exited]);
    assert!(notified.load(Ordering::SeqCst) > 0);
    assert!(s.cx.debug_bounds("meta-1-exit 0").is_some());
}

/// Issue #32: a bell where you are not looking flashes the pane, marks
/// the row and counts as needs you until the pane is seen; shells get
/// no status dot.
#[gpui::test]
fn a_bell_marks_the_agent_until_its_pane_is_seen(cx: &mut TestAppContext) {
    use crate::core::status::AgentStatus;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut shell = row(2, "/w", 2);
    shell.info.labels.insert("kind".into(), "shell".into());
    agents(&mut s, &fake, vec![row(1, "/w", 1), shell]);
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
    assert!(s.cx.debug_bounds("status-1-Idle").is_some());
    assert!(s.cx.debug_bounds("pane-1-status-Idle").is_some());
    assert!(s.cx.debug_bounds("status-2-Idle").is_none());

    let ring = |s: &mut Setup<'_>| {
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        fake.model(PaneId(1)).lock().unwrap().on_output(b"ding\x07");
        s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
        s.cx.run_until_parked();
    };
    let flashing = |s: &mut Setup<'_>| {
        s.app.read_with(s.cx, |a, cx| {
            a.pane_view(PaneId(1)).unwrap().read(cx).flashing()
        })
    };
    let marked = |s: &mut Setup<'_>| {
        s.app.read_with(s.cx, |a, _| {
            (
                a.bells.contains(&1),
                a.statuses.get(&1).copied(),
                a.needs_you_count(),
            )
        })
    };

    // Looking at it: the pane flashes, nothing is marked.
    ring(&mut s);
    assert!(flashing(&mut s));
    assert_eq!(marked(&mut s), (false, Some(AgentStatus::Idle), 0));

    // Away from the window: marked, and it needs you.
    s.app.update(s.cx, |a, cx| a.set_window_active(false, cx));
    ring(&mut s);
    assert_eq!(marked(&mut s), (true, Some(AgentStatus::NeedsYou), 1));
    assert!(s.cx.debug_bounds("status-1-Needs you").is_some());
    assert!(s.cx.debug_bounds("pane-1-status-Needs you").is_some());

    // Back at the window, with its pane focused: seen.
    s.app.update(s.cx, |a, cx| a.set_window_active(true, cx));
    s.cx.run_until_parked();
    assert_eq!(marked(&mut s), (false, Some(AgentStatus::Idle), 0));
}

/// Issue #33: each agent's colour, from its id, marks its row and its
/// pane header; shells get one too.
#[gpui::test]
fn rows_and_pane_headers_carry_the_agent_colour(cx: &mut TestAppContext) {
    use crate::core::identity::agent_colour;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut shell = row(2, "/w", 2);
    shell.info.labels.insert("kind".into(), "shell".into());
    agents(&mut s, &fake, vec![row(1, "/w", 1), shell]);
    s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
    s.cx.run_until_parked();
    let (one, two) = (agent_colour(1).name, agent_colour(2).name);
    assert_ne!(one, two);
    assert!(
        s.cx.debug_bounds(leak(format!("identity-1-{one}")))
            .is_some()
    );
    assert!(
        s.cx.debug_bounds(leak(format!("identity-2-{two}")))
            .is_some()
    );
    assert!(
        s.cx.debug_bounds(leak(format!("pane-1-identity-{one}")))
            .is_some()
    );
    assert!(s.cx.debug_bounds("identity-1-emoji").is_none());
    assert!(s.cx.debug_bounds("pane-1-identity-emoji").is_none());
}

/// `[gui] agent_emoji` puts the agent's emoji in place of the swatch
/// and the header icon.
#[gpui::test]
fn agent_emoji_replaces_the_swatch(cx: &mut TestAppContext) {
    use crate::core::identity::agent_colour;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        agent_emoji: true,
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    agents(&mut s, &fake, vec![row(1, "/w", 1)]);
    s.app.update(s.cx, |a, cx| a.show_agent(1, cx));
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("identity-1-emoji").is_some());
    assert!(s.cx.debug_bounds("pane-1-identity-emoji").is_some());
    let one = agent_colour(1).name;
    assert!(
        s.cx.debug_bounds(leak(format!("identity-1-{one}")))
            .is_none()
    );
}

/// `debug_bounds` takes a `&'static str`.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
