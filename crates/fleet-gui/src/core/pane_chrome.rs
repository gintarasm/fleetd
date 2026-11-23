//! What the workspace tells a pane about itself, and the pane's header,
//! banner, empty-state and cursor decisions drawn from it: plain data, no
//! toolkit (ADR 0029).

use std::time::Duration;

use crate::core::pane::PaneState;
use crate::core::question::OpenQuestion;
use crate::core::shortcuts::{EmptyPane, HINT, empty_pane};
use crate::core::status::{AgentStatus, DotFill};

/// `(cols, rows)` of whole cells that fit, at least 1×1.
pub fn grid_size(bounds_w: f32, bounds_h: f32, cell_w: f32, line_h: f32) -> (u16, u16) {
    let fit = |len: f32, cell: f32| {
        if cell > 0.0 && len.is_finite() {
            (len / cell).floor().clamp(1.0, f32::from(u16::MAX)) as u16
        } else {
            1
        }
    };
    (fit(bounds_w, cell_w), fit(bounds_h, line_h))
}

/// What the workspace tells a pane about itself, for its header and
/// banners.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneChrome {
    /// 1-based, in reading order.
    pub number: usize,
    /// The layout puts an agent here.
    pub has_agent: bool,
    /// The agent list says that agent exited or is lost.
    pub agent_ended: bool,
    pub program: Option<String>,
    /// The agent's directory, with `~`.
    pub cwd: Option<String>,
    /// The git checkout the directory is in, as the header shows it
    /// (`core::git::header_text`, ADR 0085).
    pub git: Option<String>,
    /// The agent's name (ADR 0052), if it has one.
    pub name: Option<String>,
    /// The agent's directory when it is on this machine: file paths in the
    /// pane resolve against it and become links (ADR 0046).
    pub link_dir: Option<String>,
    /// A spawn into this pane is on its way.
    pub starting: bool,
    /// When the control connection retries, while it is down.
    pub reconnect_in: Option<Duration>,
    /// The pane has keyboard focus (drawn in the header).
    pub focused: bool,
    /// The pane fills the window (its button offers Restore).
    pub maximized: bool,
    /// Focus mode dims this pane while it lacks focus.
    pub dims: bool,
    /// The agent's status (ADR 0042); none for a shell or no agent.
    pub status: Option<AgentStatus>,
    /// How its dot is drawn: solid when reported (ADR 0075).
    pub status_fill: DotFill,
    /// The agent list arrived and is empty: an empty pane welcomes.
    pub no_agents: bool,
    /// How long the ended agent ran, if known (the summary bar).
    pub run_time: Option<Duration>,
    /// The agent the layout puts here: its colour marks the header icon
    /// and the focus ring (ADR 0064).
    pub agent: Option<u64>,
    /// `[gui] agent_emoji`: the agent's emoji replaces the header icon.
    pub agent_emoji: bool,
    /// The agent runs on this machine (the target is local): paths from
    /// here mean the same to it, so files can be dropped and images
    /// pasted (ADR 0081).
    pub local: bool,
    /// The agent's open question, shown in a strip under the header (ADR
    /// 0078).
    pub question: Option<OpenQuestion>,
}

/// The line drawn over the grid, if any.
pub fn banner(chrome: &PaneChrome, state: PaneState) -> Option<String> {
    let disconnected = || match chrome.reconnect_in {
        Some(d) => format!(
            "disconnected — reconnecting in {}s",
            d.as_secs_f32().ceil().max(1.0) as u64
        ),
        None => "disconnected — reconnecting".to_owned(),
    };
    match state {
        PaneState::Disconnected => Some(disconnected()),
        _ if !chrome.has_agent => None,
        PaneState::Attaching => Some("attaching…".to_owned()),
        // The summary bar at the bottom says how it ended.
        PaneState::Exited(_) | PaneState::Lost | PaneState::Live => None,
    }
}

/// The summary bar offers Restart (as ⌘R does): the pane's agent exited
/// or is gone.
pub fn restartable(chrome: &PaneChrome, state: PaneState) -> bool {
    chrome.has_agent && matches!(state, PaneState::Exited(_) | PaneState::Lost)
}

/// What a pane with no agent shows.
pub fn empty_state(chrome: &PaneChrome) -> EmptyPane {
    empty_pane(chrome.has_agent, chrome.starting, chrome.no_agents)
}

/// The text centred in a pane with no agent (not the welcome).
pub fn empty_message(chrome: &PaneChrome) -> Option<&'static str> {
    match empty_state(chrome) {
        EmptyPane::Starting => Some("starting…"),
        EmptyPane::Hint => Some(HINT),
        EmptyPane::None | EmptyPane::Welcome => None,
    }
}

/// Whether the grid draws a cursor: only for an agent that may still
/// take input (none in an empty pane, or once the agent exited or is gone).
pub fn draws_cursor(chrome: &PaneChrome, state: PaneState) -> bool {
    chrome.has_agent
        && !chrome.agent_ended
        && !matches!(state, PaneState::Exited(_) | PaneState::Lost)
}

/// The header's parts (ADR 0052).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderParts {
    /// The agent's name, else the OSC title if the agent set one, else the
    /// program.
    pub title: String,
    /// `program · id` once the agent is listed; else empty.
    pub detail: String,
    /// The git repository and branch (ADR 0085); else empty.
    pub git: String,
    /// The directory, with `~`.
    pub path: String,
}

pub fn header_parts(chrome: &PaneChrome, osc_title: Option<&str>) -> HeaderParts {
    let title = chrome
        .name
        .clone()
        .or_else(|| {
            osc_title
                .filter(|t| !t.trim().is_empty())
                .map(str::to_owned)
        })
        .or_else(|| chrome.program.clone())
        .unwrap_or_default();
    let detail = match (&chrome.program, chrome.agent, &chrome.cwd) {
        // Listed (the cwd comes with the list): its program and id.
        (Some(p), Some(id), Some(_)) => format!("{p} · {id}"),
        _ => String::new(),
    };
    HeaderParts {
        title,
        detail,
        git: chrome.git.clone().unwrap_or_default(),
        path: chrome.cwd.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        HeaderParts, PaneChrome, banner, draws_cursor, empty_message, grid_size, header_parts,
    };
    use crate::core::pane::PaneState;

    #[test]
    fn resize_computes_cols_rows_from_cell_metrics() {
        assert_eq!(grid_size(800.0, 600.0, 8.0, 16.0), (100, 37));
        assert_eq!(grid_size(805.9, 607.9, 8.0, 16.0), (100, 37));
        assert_eq!(grid_size(3.0, 2.0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_size(0.0, 0.0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_size(100.0, 100.0, 0.0, 0.0), (1, 1));
    }

    #[test]
    fn empty_pane_message_goes_by_the_layout_not_the_pane_state() {
        let empty = PaneChrome::default();
        // A pane with no agent is `Attaching`; it still says "select".
        assert_eq!(banner(&empty, PaneState::Attaching), None);
        assert_eq!(empty_message(&empty), Some("Select an agent or press ⌘N"));
        let starting = PaneChrome {
            starting: true,
            ..PaneChrome::default()
        };
        assert_eq!(empty_message(&starting), Some("starting…"));
        let shown = PaneChrome {
            has_agent: true,
            ..PaneChrome::default()
        };
        assert_eq!(empty_message(&shown), None);
        assert_eq!(
            banner(&shown, PaneState::Attaching).as_deref(),
            Some("attaching…")
        );
        // The summary bar says how it ended, not a banner.
        assert_eq!(banner(&shown, PaneState::Exited(3)), None);
        assert_eq!(banner(&shown, PaneState::Lost), None);
        assert_eq!(banner(&shown, PaneState::Live), None);
        assert_eq!(
            banner(&shown, PaneState::Disconnected).as_deref(),
            Some("disconnected — reconnecting")
        );
        let down = PaneChrome {
            reconnect_in: Some(Duration::from_millis(2500)),
            ..shown.clone()
        };
        assert_eq!(
            banner(&down, PaneState::Disconnected).as_deref(),
            Some("disconnected — reconnecting in 3s")
        );
    }

    #[test]
    fn cursor_only_for_an_agent_that_runs() {
        let empty = PaneChrome::default();
        assert!(!draws_cursor(&empty, PaneState::Attaching));
        assert!(!draws_cursor(&empty, PaneState::Live));
        let shown = PaneChrome {
            has_agent: true,
            ..PaneChrome::default()
        };
        assert!(draws_cursor(&shown, PaneState::Live));
        assert!(draws_cursor(&shown, PaneState::Attaching));
        assert!(draws_cursor(&shown, PaneState::Disconnected));
        assert!(!draws_cursor(&shown, PaneState::Exited(0)));
        assert!(!draws_cursor(&shown, PaneState::Lost));
        // Known to have ended, it gets none while disconnected either.
        let ended = PaneChrome {
            agent_ended: true,
            ..shown
        };
        assert!(!draws_cursor(&ended, PaneState::Disconnected));
    }

    #[test]
    fn header_shows_the_name_else_the_osc_title_with_program_and_id() {
        let chrome = PaneChrome {
            program: Some("claude".into()),
            cwd: Some("~/dev".into()),
            agent: Some(4),
            ..PaneChrome::default()
        };
        let parts = |title: &str| HeaderParts {
            title: title.into(),
            detail: "claude · 4".into(),
            git: String::new(),
            path: "~/dev".into(),
        };
        assert_eq!(header_parts(&chrome, None), parts("claude"));
        assert_eq!(header_parts(&chrome, Some("Refactor")), parts("Refactor"));
        assert_eq!(header_parts(&chrome, Some("  ")).title, "claude");
        let named = PaneChrome {
            name: Some("lto-falcon".into()),
            ..chrome.clone()
        };
        assert_eq!(header_parts(&named, Some("Refactor")), parts("lto-falcon"));
        // Not listed yet: no detail.
        let unlisted = PaneChrome {
            program: Some("agent 4".into()),
            agent: Some(4),
            ..PaneChrome::default()
        };
        assert_eq!(header_parts(&unlisted, None).detail, "");
        // In a git checkout: its repository and branch (ADR 0085).
        let in_repo = PaneChrome {
            git: Some("fleetd (issue-97) · feat/x".into()),
            ..chrome.clone()
        };
        assert_eq!(
            header_parts(&in_repo, None).git,
            "fleetd (issue-97) · feat/x"
        );
    }
}
