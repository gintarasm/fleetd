//! Find in scrollback (issue #36): what matches, the count the bar shows,
//! and the search over a pane's history and grid in bounded steps.

use fleet_gui::core::find::{FindStatus, Needle, row_matches, status_label};
use fleet_gui::core::pane::{PaneModel, ViewRow};
use fleet_gui::core::selection::LineRef;
use fleet_proto::AttachOk;
use fleet_term::Cell;

fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell {
            ch,
            ..Cell::default()
        })
        .collect()
}

fn pane(rows: u32, tail: &[String]) -> PaneModel {
    let mut pane = PaneModel::new(fleet_gui::core::theme::DARK_PALETTE);
    pane.on_attached(&AttachOk {
        agent_id: 1,
        cols: 40,
        rows,
        history_tail: tail.iter().map(|s| s.as_bytes().to_vec().into()).collect(),
        primary: true,
        ..Default::default()
    });
    pane
}

#[test]
fn a_row_matches_case_insensitively_by_default() {
    let mut out = Vec::new();
    let needle = Needle::new("warn", false).unwrap();
    row_matches(&cells("WARN: warning"), &needle, &mut out);
    assert_eq!(out, [(0, 4), (6, 10)]);
    out.clear();
    let needle = Needle::new("warn", true).unwrap();
    row_matches(&cells("WARN: warning"), &needle, &mut out);
    assert_eq!(out, [(6, 10)]);
}

#[test]
fn the_bar_counts_matches() {
    let status = FindStatus {
        index: Some(2),
        count: 5,
        searching: false,
    };
    assert_eq!(status_label(status, true), "2 of 5");
}

#[test]
fn a_large_history_is_searched_in_steps_and_ansi_styling_does_not_split_matches() {
    let tail: Vec<String> = (0..10_000)
        .map(|i| {
            if i % 1000 == 0 {
                format!("\x1b[31mpan\x1b[1micked\x1b[0m at {i}")
            } else {
                format!("ok {i}")
            }
        })
        .collect();
    let mut pane = pane(5, &tail);
    pane.find_open("panicked", false);
    let mut steps = 0;
    while pane.find_step(500).more {
        steps += 1;
    }
    assert!(steps >= 19, "{steps}");
    let status = pane.find_status().unwrap();
    assert_eq!((status.index, status.count), (Some(10), 10));
    // The newest match is in view, highlighted as the current one.
    let mut rows = Vec::new();
    pane.view_rows(5, &mut rows);
    assert!(rows.contains(&ViewRow::History(9000)), "{rows:?}");
    let mut found = Vec::new();
    pane.find_matches(LineRef::History(9000), &mut found);
    assert_eq!(found, [(0, 8, true)]);
    pane.find_matches(LineRef::History(8000), &mut found);
    assert_eq!(found, [(0, 8, false)]);
    // Next goes up.
    assert!(pane.find_next(true));
    assert_eq!(pane.find_status().unwrap().index, Some(9));
}

#[test]
fn nothing_is_highlighted_while_find_is_closed() {
    let mut pane = pane(2, &["hit".to_owned()]);
    let mut found = vec![(0, 1, false)];
    pane.find_matches(LineRef::History(0), &mut found);
    assert!(found.is_empty());
    assert_eq!(pane.find_status(), None);
    assert!(!pane.find_pending());
    pane.find_open("hit", false);
    assert!(pane.find_pending());
}
