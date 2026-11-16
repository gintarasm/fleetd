//! The summary bar of a pane whose agent ended (issue #45): what it says
//! for a listed agent, and what Copy last output copies from a pane.

use std::time::Duration;

use fleet_gui::core::ended::{EndedSummary, Outcome, run_time};
use fleet_gui::core::pane::{PaneModel, PaneState};
use fleet_proto::{AgentInfo, AgentState, AttachOk};
use fleet_term::Palette;

fn palette() -> Palette {
    Palette {
        fg: (0xc0, 0xc0, 0xc0),
        bg: (0x10, 0x20, 0x30),
        cursor: (0xff, 0xff, 0xff),
        ansi: [(0, 0, 0); 16],
    }
}

fn exited(code: i32, created: i64, exited: Option<i64>) -> AgentInfo {
    let mut info = AgentInfo {
        id: 4,
        created_unix_ms: created,
        exit_code: Some(code),
        exited_unix_ms: exited,
        ..Default::default()
    };
    info.set_state(AgentState::Exited);
    info
}

fn summary_of(info: &AgentInfo, state: PaneState) -> Option<EndedSummary> {
    EndedSummary::of(state, run_time(info.created_unix_ms, info.exited_unix_ms))
}

#[test]
fn a_listed_failure_shows_its_code_and_run_time() {
    let info = exited(2, 10_000, Some(10_000 + 3_725_000));
    let s = summary_of(&info, PaneState::Exited(2)).unwrap();
    assert_eq!(s.outcome, Outcome::Failed(2));
    assert!(!s.succeeded());
    assert_eq!(s.headline(), "Exited · code 2");
    assert_eq!(s.run_time_text().as_deref(), Some("ran 1h 2m"));
}

#[test]
fn a_success_from_an_older_node_has_no_run_time() {
    // A node before ADR 0058 lists no exit time.
    let info = exited(0, 10_000, None);
    let s = summary_of(&info, PaneState::Exited(0)).unwrap();
    assert!(s.succeeded());
    assert_eq!(s.run_time, None);
    assert_eq!(s.run_time_text(), None);
}

#[test]
fn a_running_pane_has_no_summary() {
    let info = exited(0, 10_000, Some(20_000));
    assert_eq!(summary_of(&info, PaneState::Live), None);
    assert_eq!(
        EndedSummary::of(PaneState::Lost, Some(Duration::from_secs(5)))
            .unwrap()
            .run_time,
        None
    );
}

#[test]
fn copy_last_output_takes_the_grid_not_the_history_or_the_scrolled_view() {
    let mut pane = PaneModel::new(palette());
    pane.on_attached(&AttachOk {
        agent_id: 4,
        cols: 20,
        rows: 4,
        history_tail: vec![b"old history".to_vec().into()],
        ..Default::default()
    });
    pane.on_output(b"one\r\ntwo\r\nthree\r\nfour\r\nfive  \r\n");
    pane.on_exited(1);
    pane.scroll_lines(3);
    let rows = pane.terminal().visible_text();
    let text = fleet_gui::core::ended::last_output(&rows);
    assert_eq!(text.as_deref(), Some("three\nfour\nfive"));
}
