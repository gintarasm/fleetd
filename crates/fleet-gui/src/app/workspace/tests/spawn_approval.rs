use super::*;

#[gpui::test]
fn a_pending_spawn_shows_who_asks_and_can_be_approved_or_denied(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    agents(&mut s, &fake, vec![row(1, "/w", 1)]);
    assert!(s.cx.debug_bounds("pending-spawns").is_none());
    let spawn = |id| fleet_proto::PendingSpawn {
        id,
        parent: 1,
        program: "claude".into(),
        args: vec!["-p".into(), "go".into()],
        cwd: "/w".into(),
        ..Default::default()
    };
    *fake.pending.lock().unwrap() = vec![spawn(4), spawn(5)];
    agents(&mut s, &fake, vec![row(1, "/w", 1)]);
    assert!(s.cx.debug_bounds("pending-spawn-4").is_some());
    assert!(s.cx.debug_bounds("pending-spawn-5").is_some());
    click(&mut s, "pending-spawn-4-approve");
    click(&mut s, "pending-spawn-5-deny");
    let calls = fake.calls();
    assert!(calls.contains(&Call::Decide(4, true)), "{calls:?}");
    assert!(calls.contains(&Call::Decide(5, false)), "{calls:?}");

    // Over the graph too, where the sidebar is hidden.
    click(&mut s, "graph-toggle");
    assert!(s.ws.read_with(s.cx, |w, _| w.graph_shown()));
    let card = s.cx.debug_bounds("pending-spawn-4").unwrap();
    let toggle = s.cx.debug_bounds("graph-toggle").unwrap();
    assert!(card.top() > toggle.bottom(), "below the titlebar");

    fake.pending.lock().unwrap().clear();
    agents(&mut s, &fake, vec![row(1, "/w", 1)]);
    assert!(
        s.app.read_with(s.cx, |a, _| a.pending_spawns.is_empty()),
        "a decided spawn leaves"
    );
}

#[gpui::test]
fn a_child_spawned_while_watching_is_highlighted(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let child = |id: u64, parent: u64| {
        let mut r = row(id, "/w", id as i64);
        r.info
            .labels
            .insert("fleet.parent".to_owned(), parent.to_string());
        r
    };
    fake.listed.store(true, std::sync::atomic::Ordering::SeqCst);
    // What was there at the first list is not news.
    agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
    assert!(s.cx.debug_bounds("tab-2").is_some());
    assert!(s.cx.debug_bounds("tab-2-new").is_none());
    agents(
        &mut s,
        &fake,
        vec![row(1, "/w", 1), child(2, 1), child(3, 2), row(4, "/w", 4)],
    );
    assert!(s.cx.debug_bounds("tab-3-new").is_some());
    assert!(s.cx.debug_bounds("tab-2-new").is_none());
    assert!(s.cx.debug_bounds("tab-4-new").is_none(), "not a child");
    s.cx.simulate_keystrokes("cmd-shift-g");
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("graph-card-3-new").is_some());
    assert!(s.cx.debug_bounds("graph-card-2-new").is_none());
}
