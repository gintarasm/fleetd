use super::*;

fn peeked(s: &mut Setup<'_>) -> Option<u64> {
    s.ws.read_with(s.cx, |w, cx| w.peek().map(|p| p.read(cx).agent()))
}

#[gpui::test]
fn space_peeks_at_the_selected_row_without_changing_the_layout(cx: &mut TestAppContext) {
    use crate::core::peek::PEEK_PANE;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
    // No selection: Space starts the filter, as before.
    s.ws.update_in(s.cx, |w, window, cx| w.focus_sidebar(window, cx));
    s.cx.simulate_keystrokes("space");
    assert_eq!(peeked(&mut s), None);
    s.ws.update_in(s.cx, |w, window, cx| {
        w.filter_field
            .update(cx, |f, cx| f.set_text(String::new(), cx));
        w.focus_sidebar(window, cx);
    });
    let at = center(&mut s, "tab-2");
    s.cx.simulate_click(at, Modifiers::command());
    s.cx.run_until_parked();
    s.cx.simulate_keystrokes("space");
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), Some(2));
    assert!(s.cx.debug_bounds("peek").is_some());
    assert!(fake.calls().contains(&Call::Open(PEEK_PANE, Some(2))));
    assert_eq!(shown(&mut s, PaneId(1)), None, "the layout is unchanged");
    // Typing goes to the agent, through the peek's connection.
    s.cx.simulate_keystrokes("y");
    assert!(
        fake.pane_inputs
            .lock()
            .unwrap()
            .contains(&(PEEK_PANE, b"y".to_vec()))
    );
    // Escape closes it, and does not reach the agent.
    s.cx.simulate_keystrokes("escape");
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), None);
    assert!(fake.calls().contains(&Call::Close(PEEK_PANE)));
    assert!(
        !fake
            .pane_inputs
            .lock()
            .unwrap()
            .iter()
            .any(|(p, b)| *p == PEEK_PANE && b == b"\x1b")
    );
    assert_eq!(shown(&mut s, PaneId(1)), None);
    // ⌘Return opens it in the focused pane.
    s.ws.update_in(s.cx, |w, window, cx| w.open_peek(2, window, cx));
    s.cx.simulate_keystrokes("cmd-enter");
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), None);
    assert_eq!(shown(&mut s, PaneId(1)), Some(2));
}

#[gpui::test]
fn one_peek_at_a_time(cx: &mut TestAppContext) {
    use crate::core::peek::PEEK_PANE;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
    s.ws.update_in(s.cx, |w, window, cx| w.open_peek(1, window, cx));
    s.ws.update_in(s.cx, |w, window, cx| w.open_peek(2, window, cx));
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), Some(2));
    let opens = fake
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Open(p, _) if *p == PEEK_PANE))
        .count();
    assert_eq!(opens, 2);
    assert!(fake.calls().contains(&Call::Close(PEEK_PANE)));
    // An unknown agent is not peeked at.
    s.cx.simulate_keystrokes("escape");
    s.ws.update_in(s.cx, |w, window, cx| w.open_peek(9, window, cx));
    assert_eq!(peeked(&mut s), None);
}

#[gpui::test]
fn resting_on_a_graph_card_peeks(cx: &mut TestAppContext) {
    use crate::core::peek::HOVER_HOLD;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), row(2, "/w", 2)]);
    s.cx.simulate_keystrokes("cmd-shift-g");
    s.cx.run_until_parked();
    let at = center(&mut s, "graph-card-2");
    s.cx.simulate_mouse_move(at, None, Modifiers::none());
    s.cx.executor().advance_clock(HOVER_HOLD / 2);
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), None, "not yet");
    s.cx.executor().advance_clock(HOVER_HOLD);
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), Some(2));
    // Escape goes back to the graph, still shown.
    s.cx.simulate_keystrokes("escape");
    s.cx.run_until_parked();
    assert_eq!(peeked(&mut s), None);
    assert!(s.cx.debug_bounds("graph-card-2").is_some());
}
