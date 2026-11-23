use super::*;

fn exit_ids(s: &mut Setup<'_>) -> Vec<u64> {
    s.app
        .read_with(s.cx, |a, _| a.exit_toasts().map(|n| n.agent_id).collect())
}

/// [`setup`] with motion reduced, for tests that click a toast. gpui
/// times the slide-in by the wall clock, and the test window redraws
/// only on an event: under load, the click's mouse down redraws a toast
/// first drawn still offset at its final place, 16 px away, and the
/// mouse up then misses the button.
fn setup_still<'a>(cx: &'a mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Setup<'a> {
    let config = GuiConfig {
        reduce_motion: MotionChoice::On,
        ..GuiConfig::default()
    };
    setup_with(cx, fake, dir, config)
}

#[gpui::test]
fn an_exit_shows_a_toast_with_its_code_and_restart(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup_still(cx, &fake, dir.path());
    // First seen already exited: not news.
    agents(&mut s, &fake, vec![exited(6, 1), row(7, "/w", 1)]);
    assert!(exit_ids(&mut s).is_empty());
    agents(&mut s, &fake, vec![exited(6, 1), exited(7, 3)]);
    assert_eq!(exit_ids(&mut s), vec![7]);
    let text = s
        .app
        .read_with(s.cx, |a, _| a.exit_toasts().next().unwrap().text());
    assert_eq!(text, "sh exited with code 3");
    assert!(s.cx.debug_bounds("exit-toast-7").is_some());
    assert!(
        s.cx.debug_bounds("exit-toast-7-celebration").is_none(),
        "no celebration for a failure, nor while it is off"
    );
    click(&mut s, "exit-toast-7-restart");
    assert!(fake.calls().contains(&Call::Restart(7)));
    assert!(exit_ids(&mut s).is_empty(), "Restart takes the toast away");
}

#[gpui::test]
fn exit_toasts_close_go_on_their_own_and_follow_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup_still(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![row(1, "/w", 1), row(2, "/w", 2), row(3, "/w", 3)],
    );
    agents(
        &mut s,
        &fake,
        vec![exited(1, 0), exited(2, 0), exited(3, 0)],
    );
    assert_eq!(exit_ids(&mut s), vec![1, 2, 3]);
    click(&mut s, "exit-toast-1-close");
    assert_eq!(exit_ids(&mut s), vec![2, 3]);
    // Agent 2 restarted elsewhere: it left the list, its toast goes.
    agents(&mut s, &fake, vec![exited(3, 0)]);
    assert_eq!(exit_ids(&mut s), vec![3]);
    s.cx.executor()
        .advance_clock(feedback::EXIT_TOAST_TTL + Duration::from_millis(10));
    s.cx.run_until_parked();
    assert!(exit_ids(&mut s).is_empty(), "the last one timed out");
}

#[gpui::test]
fn celebration_needs_the_setting_and_code_zero(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        celebrate: true,
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    agents(&mut s, &fake, vec![row(7, "/w", 1), row(8, "/w", 2)]);
    agents(&mut s, &fake, vec![exited(7, 0), exited(8, 2)]);
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("exit-toast-7-celebration").is_some());
    assert!(s.cx.debug_bounds("exit-toast-8-celebration").is_none());
}
