//! The summary at the bottom of a pane whose agent ended (issue #45): how
//! it ended, how long it ran, and the text its Copy last output button
//! copies. Plain data in, plain data out (ADR 0029).

use std::time::Duration;

use super::pane::PaneState;

/// How the pane's agent ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit code 0.
    Succeeded,
    /// Any other exit code.
    Failed(i32),
    /// The node no longer has the agent; how it ended is unknown.
    Lost,
}

/// What the summary bar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndedSummary {
    pub outcome: Outcome,
    /// `None` when the exit time is unknown (a lost agent, or an exit the
    /// node recorded before it stored exit times).
    pub run_time: Option<Duration>,
}

impl EndedSummary {
    /// The summary for a pane in `state`, if its agent ended. `run_time`
    /// comes from [`run_time`]; a lost agent never shows one.
    pub fn of(state: PaneState, run_time: Option<Duration>) -> Option<Self> {
        let outcome = match state {
            PaneState::Exited(0) => Outcome::Succeeded,
            PaneState::Exited(code) => Outcome::Failed(code),
            PaneState::Lost => Outcome::Lost,
            PaneState::Attaching | PaneState::Live | PaneState::Disconnected => return None,
        };
        let run_time = run_time.filter(|_| outcome != Outcome::Lost);
        Some(EndedSummary { outcome, run_time })
    }

    /// Drawn in the success style (else the failure style).
    pub fn succeeded(&self) -> bool {
        self.outcome == Outcome::Succeeded
    }

    /// The bar's main text.
    pub fn headline(&self) -> String {
        match self.outcome {
            Outcome::Succeeded => "Exited · code 0".to_owned(),
            Outcome::Failed(code) => format!("Exited · code {code}"),
            Outcome::Lost => "The agent is gone".to_owned(),
        }
    }

    /// "ran 3m 12s", if the run time is known.
    pub fn run_time_text(&self) -> Option<String> {
        self.run_time.map(|d| format!("ran {}", format_run_time(d)))
    }
}

/// How long an agent ran, from the list's `created_unix_ms` and
/// `exited_unix_ms`; `None` without an exit time or when the clocks
/// disagree (an exit before the start).
pub fn run_time(created_unix_ms: i64, exited_unix_ms: Option<i64>) -> Option<Duration> {
    let ms = exited_unix_ms?.checked_sub(created_unix_ms)?;
    u64::try_from(ms).ok().map(Duration::from_millis)
}

/// A run time in the two largest units: "42s", "3m 12s", "2h 5m", "3d 4h".
/// Under a second is "0s".
pub fn format_run_time(d: Duration) -> String {
    let s = d.as_secs();
    let (m, h, days) = (s / 60, s / 3600, s / 86_400);
    if days > 0 {
        format!("{days}d {}h", h % 24)
    } else if h > 0 {
        format!("{h}h {}m", m % 60)
    } else if m > 0 {
        format!("{m}m {}s", s % 60)
    } else {
        format!("{s}s")
    }
}

/// The last screenful as text to copy: the grid's rows (as
/// `Terminal::visible_text` gives them) with trailing spaces and the blank
/// rows above and below the output dropped. `None` for a blank screen.
pub fn last_output(rows: &[String]) -> Option<String> {
    let blank = |r: &String| r.trim().is_empty();
    let first = rows.iter().position(|r| !blank(r))?;
    let last = rows.iter().rposition(|r| !blank(r))?;
    let lines: Vec<&str> = rows[first..=last].iter().map(|r| r.trim_end()).collect();
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn only_an_ended_pane_has_a_summary() {
        for state in [
            PaneState::Attaching,
            PaneState::Live,
            PaneState::Disconnected,
        ] {
            assert_eq!(EndedSummary::of(state, Some(Duration::ZERO)), None);
        }
        let ok = EndedSummary::of(PaneState::Exited(0), None).unwrap();
        assert_eq!(ok.outcome, Outcome::Succeeded);
        assert!(ok.succeeded());
        let failed = EndedSummary::of(PaneState::Exited(3), None).unwrap();
        assert_eq!(failed.outcome, Outcome::Failed(3));
        assert!(!failed.succeeded());
        let lost = EndedSummary::of(PaneState::Lost, None).unwrap();
        assert_eq!(lost.outcome, Outcome::Lost);
        assert!(!lost.succeeded());
    }

    #[test]
    fn headline_names_the_exit_code() {
        let text = |s| EndedSummary::of(s, None).unwrap().headline();
        assert_eq!(text(PaneState::Exited(0)), "Exited · code 0");
        assert_eq!(text(PaneState::Exited(137)), "Exited · code 137");
        assert_eq!(text(PaneState::Exited(-1)), "Exited · code -1");
        assert_eq!(text(PaneState::Lost), "The agent is gone");
    }

    #[test]
    fn run_time_shows_when_known_and_never_for_a_lost_agent() {
        let d = Some(Duration::from_secs(192));
        let s = EndedSummary::of(PaneState::Exited(1), d).unwrap();
        assert_eq!(s.run_time_text().as_deref(), Some("ran 3m 12s"));
        let s = EndedSummary::of(PaneState::Exited(1), None).unwrap();
        assert_eq!(s.run_time_text(), None);
        let s = EndedSummary::of(PaneState::Lost, d).unwrap();
        assert_eq!(s.run_time, None);
    }

    #[test]
    fn run_time_from_the_list() {
        assert_eq!(
            run_time(1_000, Some(3_500)),
            Some(Duration::from_millis(2_500))
        );
        assert_eq!(run_time(1_000, Some(1_000)), Some(Duration::ZERO));
        assert_eq!(run_time(1_000, None), None);
        // Clocks that disagree show no run time rather than a wrong one.
        assert_eq!(run_time(5_000, Some(1_000)), None);
        assert_eq!(run_time(i64::MIN, Some(i64::MAX)), None);
    }

    #[test]
    fn run_time_in_its_two_largest_units() {
        let f = |s| format_run_time(Duration::from_secs(s));
        assert_eq!(format_run_time(Duration::from_millis(400)), "0s");
        assert_eq!(f(42), "42s");
        assert_eq!(f(60), "1m 0s");
        assert_eq!(f(192), "3m 12s");
        assert_eq!(f(3_600 * 2 + 5 * 60 + 9), "2h 5m");
        assert_eq!(f(86_400 * 3 + 3_600 * 4 + 1), "3d 4h");
    }

    #[test]
    fn last_output_trims_blank_rows_and_trailing_spaces() {
        let screen = rows(&["", "  ", "$ make  ", "  ok", "", "done", "", " "]);
        assert_eq!(
            last_output(&screen).as_deref(),
            Some("$ make\n  ok\n\ndone")
        );
        assert_eq!(last_output(&rows(&["", "   "])), None);
        assert_eq!(last_output(&[]), None);
    }
}
