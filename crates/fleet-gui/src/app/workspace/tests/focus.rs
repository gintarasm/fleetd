use super::*;

#[gpui::test]
fn panes_report_focus_moving_between_them_and_window_activation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    s.cx.dispatch_action(SplitRight);
    s.cx.run_until_parked();
    assert_eq!(focused(&mut s), PaneId(2));
    // Both programs ask for focus reports (DECSET 1004).
    for pane in [1, 2] {
        let model = fake.model(PaneId(pane));
        model.lock().unwrap().on_output(b"\x1b[?1004h");
    }
    let take = |fake: &Fake| {
        let mut sent = std::mem::take(&mut *fake.pane_inputs.lock().unwrap());
        sent.sort_by_key(|(pane, _)| pane.0);
        sent
    };
    take(&fake);
    let (focus_in, focus_out) = (b"\x1b[I".to_vec(), b"\x1b[O".to_vec());

    s.cx.dispatch_action(FocusPane1);
    s.cx.run_until_parked();
    assert_eq!(
        take(&fake),
        vec![
            (PaneId(1), focus_in.clone()),
            (PaneId(2), focus_out.clone())
        ]
    );

    // Only the focused pane hears of the window's activation.
    s.cx.deactivate_window();
    assert_eq!(take(&fake), vec![(PaneId(1), focus_out.clone())]);
    s.cx.update(|window, _| window.activate_window());
    s.cx.run_until_parked();
    assert_eq!(take(&fake), vec![(PaneId(1), focus_in.clone())]);

    // A pane whose program did not ask hears nothing.
    let model = fake.model(PaneId(1));
    model.lock().unwrap().on_output(b"\x1b[?1004l");
    s.cx.dispatch_action(FocusPane2);
    s.cx.run_until_parked();
    assert_eq!(take(&fake), vec![(PaneId(2), focus_in)]);
}

#[gpui::test]
fn focus_mode_dims_the_panes_without_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    // Reduced motion: the dim is there at once, no cross-fade to wait for.
    let config = GuiConfig {
        reduce_motion: MotionChoice::On,
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    s.cx.dispatch_action(SplitRight);
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("pane-1-dim").is_none(), "off by default");
    s.app.update(s.cx, |a, cx| {
        a.change_settings(vec![Change::FocusMode(true)], cx)
            .unwrap();
    });
    s.cx.run_until_parked();
    assert_eq!(focused(&mut s), PaneId(2));
    assert!(s.cx.debug_bounds("pane-1-dim").is_some(), "applies live");
    assert!(
        s.cx.debug_bounds("pane-2-dim").is_none(),
        "the focused one is clear"
    );
    s.cx.dispatch_action(FocusPane1);
    s.cx.run_until_parked();
    assert!(s.cx.debug_bounds("pane-2-dim").is_some());
    // (gpui keeps the bounds of elements once drawn: the chrome says
    // which pane is dimmed from here on.)
    let chrome = |s: &mut Setup<'_>, pane: u32| {
        s.app.read_with(s.cx, |a, cx| {
            let c = &a.pane_view(PaneId(pane)).unwrap().read(cx).chrome;
            (c.dims, c.focused)
        })
    };
    assert_eq!(chrome(&mut s, 1), (true, true), "focused: clear");
    assert_eq!(chrome(&mut s, 2), (true, false), "dimmed");
}

#[gpui::test]
fn no_pane_eases_in_at_launch_or_under_reduce_motion(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let config = GuiConfig {
        reduce_motion: MotionChoice::On,
        ..GuiConfig::default()
    };
    let mut s = setup_with(cx, &fake, dir.path(), config);
    assert!(s.ws.read_with(s.cx, |w, _| w.easing_panes()).is_empty());
    s.cx.dispatch_action(SplitRight);
    s.cx.run_until_parked();
    assert_eq!(panes(&mut s).len(), 2);
    assert!(s.ws.read_with(s.cx, |w, _| w.easing_panes()).is_empty());
    assert!(s.cx.update(|_, cx| crate::app::controls::reduces_motion(cx)));
}
