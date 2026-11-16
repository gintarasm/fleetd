//! What a sidebar row shows besides its name (issue #41): the agent's
//! last output line under it, and on the right how long a running agent
//! has run or how an ended one ended.

use fleet_proto::AgentState;

use super::session::AgentRow;

/// The row's second line: the last line of `last_line`, with control
/// characters dropped and runs of whitespace (tabs included) shown as one
/// space.
pub fn preview_line(last_line: &str) -> String {
    let line = last_line
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    line.split(|c: char| c.is_whitespace())
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compact run time: `42s`, `12m`, `1h 5m`, `2d 3h`.
pub fn run_time(created_unix_ms: i64, now_ms: i64) -> String {
    let s = ((now_ms - created_unix_ms) / 1000).max(0);
    let (big, small) = match s {
        0..=59 => return format!("{s}s"),
        60..=3599 => return format!("{}m", s / 60),
        3600..=86399 => ((s / 3600, 'h'), (s % 3600 / 60, 'm')),
        _ => ((s / 86400, 'd'), (s % 86400 / 3600, 'h')),
    };
    if small.0 == 0 {
        format!("{}{}", big.0, big.1)
    } else {
        format!("{}{} {}{}", big.0, big.1, small.0, small.1)
    }
}

/// The right-hand side of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub text: String,
    /// Drawn in the danger colour: a non-zero exit, or lost.
    pub failed: bool,
}

/// Run time while running; the exit code once exited; `lost` when lost.
pub fn row_meta(row: &AgentRow, now_ms: i64) -> RowMeta {
    let (text, failed) = match (row.info.state(), row.info.exit_code) {
        (AgentState::Running, _) => (run_time(row.info.created_unix_ms, now_ms), false),
        (AgentState::Exited, Some(code)) => (format!("exit {code}"), code != 0),
        (AgentState::Exited, None) => ("exited".to_owned(), true),
        (AgentState::Lost | AgentState::Unspecified, _) => ("lost".to_owned(), true),
    };
    RowMeta { text, failed }
}

/// `new` differs from `old` only in the agents' activity (last line and
/// last-output time): same agents, in the same order, with the same info.
/// Such a change waits for the sidebar's one-second tick to be drawn, so a
/// row redraws at most once a second however fast its agent prints.
pub fn only_activity_changed(old: &[AgentRow], new: &[AgentRow]) -> bool {
    !old.is_empty() && old.len() == new.len() && old.iter().zip(new).all(|(a, b)| a.info == b.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn row(state: AgentState, code: Option<i32>, created: i64) -> AgentRow {
        let mut info = AgentInfo {
            id: 1,
            exit_code: code,
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(state);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn preview_is_the_last_line_cleaned() {
        assert_eq!(preview_line(""), "");
        assert_eq!(preview_line("  compiling fleetd  "), "compiling fleetd");
        assert_eq!(preview_line("first\nsecond\n"), "second");
        assert_eq!(preview_line("a\tb    c"), "a b c");
        assert_eq!(preview_line("bell\x07 and\x1b esc"), "bell and esc");
        assert_eq!(preview_line("ąčę ❯"), "ąčę ❯");
        assert_eq!(preview_line("\r\n"), "");
    }

    #[test]
    fn run_time_formats() {
        assert_eq!(run_time(0, 0), "0s");
        assert_eq!(run_time(0, 42_999), "42s");
        assert_eq!(run_time(0, 60_000), "1m");
        assert_eq!(run_time(0, 12 * 60_000 + 59_000), "12m");
        assert_eq!(run_time(0, 3_600_000), "1h");
        assert_eq!(run_time(0, 3_600_000 + 5 * 60_000), "1h 5m");
        assert_eq!(run_time(0, 2 * 86_400_000), "2d");
        assert_eq!(
            run_time(0, 2 * 86_400_000 + 3 * 3_600_000 + 60_000),
            "2d 3h"
        );
        // A clock behind the daemon's never shows a negative time.
        assert_eq!(run_time(10_000, 5_000), "0s");
    }

    #[test]
    fn meta_by_state() {
        let now = 125_000;
        assert_eq!(
            row_meta(&row(AgentState::Running, None, 0), now),
            RowMeta {
                text: "2m".into(),
                failed: false
            }
        );
        assert_eq!(
            row_meta(&row(AgentState::Exited, Some(0), 0), now),
            RowMeta {
                text: "exit 0".into(),
                failed: false
            }
        );
        assert_eq!(
            row_meta(&row(AgentState::Exited, Some(2), 0), now),
            RowMeta {
                text: "exit 2".into(),
                failed: true
            }
        );
        // Killed by a signal: no code.
        assert_eq!(
            row_meta(&row(AgentState::Exited, None, 0), now),
            RowMeta {
                text: "exited".into(),
                failed: true
            }
        );
        assert_eq!(
            row_meta(&row(AgentState::Lost, None, 0), now),
            RowMeta {
                text: "lost".into(),
                failed: true
            }
        );
    }

    #[test]
    fn activity_alone_is_told_apart() {
        let a = row(AgentState::Running, None, 1);
        let mut b = row(AgentState::Running, None, 2);
        b.info.id = 2;
        let old = vec![a.clone(), b.clone()];

        let mut printed = old.clone();
        printed[0].last_line = "new".into();
        printed[1].last_output_unix_ms = Some(9);
        assert!(only_activity_changed(&old, &printed));
        assert!(only_activity_changed(&old, &old));

        let mut exited = old.clone();
        exited[0].info.set_state(AgentState::Exited);
        assert!(!only_activity_changed(&old, &exited));

        let mut relabelled = old.clone();
        relabelled[1].info.labels.insert("name".into(), "x".into());
        assert!(!only_activity_changed(&old, &relabelled));

        assert!(!only_activity_changed(&old, &old[..1]));
        assert!(!only_activity_changed(&old, &[b, a]));
        // The first list is never "only activity".
        assert!(!only_activity_changed(&[], &old));
    }
}
