//! The graph view (⌘⇧G, ADR 0073).

use super::*;

use crate::app::graph_view::GraphView;
use fleet_client::tree::PARENT_LABEL;

fn child(id: u64, parent: u64) -> AgentRow {
    let mut r = row(id, "/w", id as i64);
    r.info
        .labels
        .insert(PARENT_LABEL.to_owned(), parent.to_string());
    r
}

fn ended_child(id: u64, parent: u64) -> AgentRow {
    let mut r = child(id, parent);
    r.info.set_state(AgentState::Exited);
    r.info.exit_code = Some(0);
    r
}

fn graph(s: &mut Setup<'_>) -> Entity<GraphView> {
    s.ws.read_with(s.cx, |w, _| w.graph().clone())
}

fn shown(s: &mut Setup<'_>) -> bool {
    s.ws.read_with(s.cx, |w, _| w.graph_shown())
}

fn selected(s: &mut Setup<'_>) -> Option<u64> {
    let g = graph(s);
    g.read_with(s.cx, |g, _| g.selected())
}

/// The last frame built `id`'s card (gpui's debug selectors outlive
/// the frame that drew them).
fn drawn(s: &mut Setup<'_>, id: u64) -> bool {
    s.cx.run_until_parked();
    let g = graph(s);
    s.ws.read_with(s.cx, |w, _| w.graph_shown()) && g.read_with(s.cx, |g, _| g.built.contains(&id))
}

fn forgets(fake: &Fake) -> Vec<Vec<u64>> {
    fake.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Forget(ids) => Some(ids),
            _ => None,
        })
        .collect()
}

fn open_graph<'a>(
    cx: &'a mut TestAppContext,
    fake: &Arc<Fake>,
    dir: &Path,
    rows: Vec<AgentRow>,
) -> Setup<'a> {
    let mut s = setup(cx, fake, dir);
    agents(&mut s, fake, rows);
    s.cx.simulate_keystrokes("cmd-shift-g");
    s.cx.run_until_parked();
    assert!(shown(&mut s));
    s
}

#[gpui::test]
fn cmd_shift_g_switches_to_the_graph_and_esc_back(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
    assert!(s.cx.debug_bounds("tab-1").is_some());
    // From a focused terminal (no find bar open).
    s.cx.simulate_keystrokes("cmd-shift-g");
    assert!(shown(&mut s));
    assert!(drawn(&mut s, 1) && drawn(&mut s, 2));
    assert!(s.cx.debug_bounds("tab-1").is_none(), "no sidebar");
    s.cx.simulate_keystrokes("escape");
    s.cx.run_until_parked();
    assert!(!shown(&mut s));
    assert!(s.cx.debug_bounds("tab-1").is_some());
    assert!(!drawn(&mut s, 1));
    // The titlebar button toggles it too.
    click(&mut s, "graph-toggle");
    assert!(shown(&mut s));
    click(&mut s, "graph-toggle");
    assert!(!shown(&mut s));
}

#[gpui::test]
fn hovering_a_message_line_explains_it_and_a_click_selects_the_sender(cx: &mut TestAppContext) {
    use crate::core::message_lines::{Edge, Kind, route_all};
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = open_graph(
        cx,
        &fake,
        dir.path(),
        vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
    );
    let now = now_ms();
    s.app.update(s.cx, |a, cx| {
        a.message_edges = vec![Edge {
            id: 1,
            from: 3,
            to: 1,
            kind: Kind::Message,
            at_ms: now - 2 * 60_000,
        }];
        cx.notify();
    });
    s.cx.run_until_parked();
    let g = graph(&mut s);
    let (route, vp) = g.read_with(s.cx, |g, _| {
        (route_all(g.layout(), &[(3, 1)])[0].clone(), g.viewport())
    });
    // The middle of the line's run along the column gap, on screen.
    let (a, b) = (route.points[1], route.points[2]);
    let (x, y) = vp.to_screen(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0));
    let area = s.cx.debug_bounds("graph-area").unwrap().origin;
    let at = point(area.x + gpui::px(x + 2.0), area.y + gpui::px(y));
    s.cx.simulate_mouse_move(at, None, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(g.read_with(s.cx, |g, _| g.hovered_line()), Some((3, 1)));
    assert!(s.cx.debug_bounds("graph-line-tip-3-1").is_some());
    // Over a card: the card, not the line.
    let card = s.cx.debug_bounds("graph-card-2").unwrap().center();
    s.cx.simulate_mouse_move(card, None, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(g.read_with(s.cx, |g, _| g.hovered_line()), None);
    // Back on the line, a click selects the sender and stays in the
    // graph.
    s.cx.simulate_mouse_move(at, None, Modifiers::none());
    s.cx.simulate_click(at, Modifiers::none());
    s.cx.run_until_parked();
    assert_eq!(selected(&mut s), Some(3));
    assert!(shown(&mut s));
}

#[gpui::test]
fn enter_opens_the_selected_agent_in_the_focused_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = open_graph(
        cx,
        &fake,
        dir.path(),
        vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
    );
    assert_eq!(selected(&mut s), Some(1));
    s.cx.simulate_keystrokes("right");
    assert_eq!(selected(&mut s), Some(2));
    s.cx.simulate_keystrokes("down");
    assert_eq!(selected(&mut s), Some(3));
    s.cx.simulate_keystrokes("left");
    assert_eq!(selected(&mut s), Some(1));
    // ⏎ opens the terminal panel; ⌘⏎ opens in the main view
    // (ADR 0105).
    s.cx.simulate_keystrokes("right down cmd-enter");
    s.cx.run_until_parked();
    assert!(!shown(&mut s));
    let pane = focused(&mut s);
    let agent = s.app.read_with(s.cx, |a, _| a.layout().agent(pane));
    assert_eq!(agent, Some(3));
    assert!(fake.calls().contains(&Call::Attach(pane, 3)));
}

#[gpui::test]
fn opening_an_agent_already_in_a_pane_focuses_that_pane(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![row(1, "/w", 1), child(2, 1)]);
    let first = focused(&mut s);
    s.ws.update_in(s.cx, |w, window, cx| w.show_agent(1, window, cx));
    s.cx.simulate_keystrokes("cmd-d");
    let second = focused(&mut s);
    assert_ne!(first, second);
    let before = fake.calls().len();
    s.cx.simulate_keystrokes("cmd-shift-g");
    double_click(&mut s, "graph-card-1");
    assert!(!shown(&mut s));
    assert_eq!(focused(&mut s), first);
    let agent = s.app.read_with(s.cx, |a, _| a.layout().agent(second));
    assert_eq!(agent, None, "the other pane is not replaced");
    assert!(
        !fake.calls()[before..]
            .iter()
            .any(|c| matches!(c, Call::Attach(p, _) if *p == second)),
        "nothing attached in the other pane"
    );
}

#[gpui::test]
fn backspace_stops_the_subtree_deepest_first_after_asking(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let rows = vec![
        row(1, "/w", 1),
        child(2, 1),
        child(3, 2),
        ended_child(4, 1),
        row(9, "/w", 9),
    ];
    let mut s = open_graph(cx, &fake, dir.path(), rows);
    s.cx.simulate_keystrokes("backspace");
    assert!(s.cx.has_pending_prompt());
    s.cx.simulate_prompt_answer("Cancel");
    s.cx.run_until_parked();
    assert!(kills(&fake).is_empty());
    s.cx.simulate_keystrokes("backspace");
    s.cx.simulate_prompt_answer("Stop");
    s.cx.run_until_parked();
    assert_eq!(kills(&fake), [3, 2, 1]);
    assert!(shown(&mut s), "the graph stays");
}

#[gpui::test]
fn shift_backspace_removes_only_an_ended_subtree(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut five = ended(5, "/w", 5, AgentState::Exited);
    five.info.exit_code = Some(0);
    let rows = vec![row(1, "/w", 1), ended_child(2, 1), five, ended_child(6, 5)];
    let mut s = open_graph(cx, &fake, dir.path(), rows);
    // 1 runs: nothing.
    s.cx.simulate_keystrokes("shift-backspace");
    assert!(!s.cx.has_pending_prompt());
    assert!(forgets(&fake).is_empty());
    // 2 alone: removed without a question.
    s.cx.simulate_keystrokes("right shift-backspace");
    s.cx.run_until_parked();
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(forgets(&fake), [vec![2]]);
    // 5 and 6: asked first, then one ForgetAgents.
    s.cx.simulate_keystrokes("left down shift-backspace");
    assert_eq!(selected(&mut s), Some(5));
    assert!(s.cx.has_pending_prompt());
    s.cx.simulate_prompt_answer("Remove");
    s.cx.run_until_parked();
    assert_eq!(forgets(&fake), [vec![2], vec![5, 6]]);
    assert!(kills(&fake).is_empty());
}

#[gpui::test]
fn c_collapses_a_subtree(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = open_graph(
        cx,
        &fake,
        dir.path(),
        vec![row(1, "/w", 1), child(2, 1), child(3, 1)],
    );
    assert!(drawn(&mut s, 2));
    s.cx.simulate_keystrokes("c");
    assert!(!drawn(&mut s, 2) && !drawn(&mut s, 3) && drawn(&mut s, 1));
    let g = graph(&mut s);
    let card = g
        .read_with(s.cx, |g, _| g.layout().card(1).cloned())
        .unwrap();
    assert!(card.collapsed && card.children == 2);
    s.cx.simulate_keystrokes("c");
    assert!(drawn(&mut s, 2));
}

#[gpui::test]
fn activity_redraws_without_laying_out_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let rows = vec![row(1, "/w", 1), child(2, 1)];
    let mut s = open_graph(cx, &fake, dir.path(), rows.clone());
    let g = graph(&mut s);
    let layouts = g.read_with(s.cx, |g, _| g.layouts);
    let mut busy = rows.clone();
    busy[1].last_line = "compiling…".into();
    busy[1].last_output_unix_ms = Some(now_ms());
    agents(&mut s, &fake, busy);
    assert_eq!(g.read_with(s.cx, |g, _| g.layouts), layouts);
    let mut more = rows;
    more.push(child(3, 2));
    agents(&mut s, &fake, more);
    assert_eq!(g.read_with(s.cx, |g, _| g.layouts), layouts + 1);
    assert!(drawn(&mut s, 3));
}

#[gpui::test]
fn slash_filters_and_keeps_ancestors(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut reviewer = child(3, 2);
    reviewer
        .info
        .labels
        .insert("role".into(), "reviewer".into());
    let rows = vec![row(1, "/w", 1), child(2, 1), reviewer, row(4, "/w", 4)];
    let mut s = open_graph(cx, &fake, dir.path(), rows);
    s.cx.simulate_keystrokes("/");
    s.cx.run_until_parked();
    s.cx.simulate_input("role=reviewer");
    s.cx.run_until_parked();
    assert!(drawn(&mut s, 1) && drawn(&mut s, 2) && drawn(&mut s, 3));
    assert!(!drawn(&mut s, 4));
    // Esc clears the filter, a second gives the keys back.
    s.cx.simulate_keystrokes("escape");
    assert!(drawn(&mut s, 4));
    s.cx.simulate_keystrokes("escape escape");
    s.cx.run_until_parked();
    assert!(!shown(&mut s));
}

#[gpui::test]
fn zoom_keys_scale_the_graph_not_the_terminal_font(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = open_graph(cx, &fake, dir.path(), vec![row(1, "/w", 1)]);
    let font = s.app.read_with(s.cx, |a, _| a.shown_font_size());
    s.cx.simulate_keystrokes("cmd-=");
    let g = graph(&mut s);
    let zoom = g.read_with(s.cx, |g, _| g.viewport().zoom);
    assert!((zoom - 1.2).abs() < 1e-4, "{zoom}");
    assert_eq!(s.app.read_with(s.cx, |a, _| a.shown_font_size()), font);
    s.cx.simulate_keystrokes("cmd-0");
    assert_eq!(g.read_with(s.cx, |g, _| g.viewport().zoom), 1.0);
}

#[gpui::test]
fn only_cards_in_view_are_built(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let rows: Vec<AgentRow> = (1..=80).map(|i| row(i, "/w", i as i64)).collect();
    let mut s = open_graph(cx, &fake, dir.path(), rows);
    assert!(drawn(&mut s, 1));
    assert!(!drawn(&mut s, 80));
    let g = graph(&mut s);
    let built = g.read_with(s.cx, |g, _| g.built.len());
    assert!(built > 0 && built < 40, "{built}");
    // Arrows down to the last card scroll it into view.
    for _ in 0..79 {
        s.cx.simulate_keystrokes("down");
    }
    assert_eq!(selected(&mut s), Some(80));
    assert!(drawn(&mut s, 80) && !drawn(&mut s, 1));
}
