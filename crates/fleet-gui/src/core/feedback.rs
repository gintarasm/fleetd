//! Motion and feedback decisions (issue #44, ADR 0065): pane easing,
//! the focus ring's cross-fade, focus-mode dimming, copy feedback, exit
//! toasts and the optional celebration. Toolkit-free (ADR 0029): the
//! shell asks "how far along" and "what to show", and draws it.
//!
//! Nothing here runs on its own: a [`Tween`] is a start time and a
//! length, and the shell requests frames only while one is running, so
//! an idle window draws nothing.

use std::time::{Duration, Instant};

use super::session::{AgentRow, PaneId};
use super::sidebar::program_name;
use super::style::motion::ease_out;
use fleet_proto::AgentState;

/// A new pane easing into place.
pub const PANE_IN_MS: u64 = 180;
/// How far (px, each side) a new pane grows from while it fades in.
pub const PANE_IN_INSET: f32 = 6.0;
/// The focus ring (and the focus-mode dim) moving to another pane.
pub const FOCUS_FADE_MS: u64 = 150;
/// How long "Copied" shows.
pub const COPIED_MS: u64 = 900;
/// The selection's glow after a copy.
pub const GLOW_MS: u64 = 400;
/// The celebration's burst.
pub const CELEBRATE_MS: u64 = 700;
/// How long an exit toast stays before it goes on its own.
pub const EXIT_TOAST_TTL: Duration = Duration::from_secs(8);
/// At most this many exit toasts at once (the oldest go first).
pub const MAX_EXIT_TOASTS: usize = 3;
/// The veil over a pane without focus in focus mode (its alpha).
pub const DIM_ALPHA: f32 = 0.35;

/// A one-shot 0 → 1 transition of `ms` milliseconds from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tween {
    start: Instant,
    ms: u64,
}

impl Tween {
    pub fn new(start: Instant, ms: u64) -> Self {
        Tween { start, ms }
    }

    /// Linear progress in 0..=1.
    pub fn progress(&self, now: Instant) -> f32 {
        if self.ms == 0 {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f32();
        (elapsed / (self.ms as f32 / 1000.0)).clamp(0.0, 1.0)
    }

    /// Progress eased out (fast start, gentle stop).
    pub fn value(&self, now: Instant) -> f32 {
        ease_out(self.progress(now))
    }

    /// Still moving: the shell asks for another frame.
    pub fn running(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }
}

/// The panes drawn now that were not drawn before: they ease in. None on
/// the first frame (the window opening is not a pane opening).
pub fn entering(before: &[PaneId], now: &[PaneId]) -> Vec<PaneId> {
    if before.is_empty() {
        return Vec::new();
    }
    now.iter()
        .copied()
        .filter(|p| !before.contains(p))
        .collect()
}

/// How strongly a pane's focus ring is drawn, 0..=1: fading in on the
/// pane that took focus, out on the one that lost it, with `fade` the
/// eased progress of the last change (`None`: settled).
pub fn ring(focused: bool, fade: Option<f32>) -> f32 {
    let t = fade.unwrap_or(1.0).clamp(0.0, 1.0);
    if focused { t } else { 1.0 - t }
}

/// Focus mode dims a pane while it lacks focus, when there is another
/// pane to look at instead.
pub fn dims(focus_mode: bool, drawn_panes: usize) -> bool {
    focus_mode && drawn_panes > 1
}

/// The dim veil's alpha for a pane drawn with focus-ring strength `ring`.
pub fn dim_alpha(dims: bool, ring: f32) -> f32 {
    if dims {
        DIM_ALPHA * (1.0 - ring.clamp(0.0, 1.0))
    } else {
        0.0
    }
}

/// The "Copied" flash's alpha at linear progress `t`: shown at once,
/// held, then fading over the last third.
pub fn flash_alpha(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    const FADE_FROM: f32 = 2.0 / 3.0;
    if t <= FADE_FROM {
        1.0
    } else {
        1.0 - (t - FADE_FROM) / (1.0 - FADE_FROM)
    }
}

/// The selection's glow after a copy, 1 → 0 with eased progress `t`.
pub fn glow(t: f32) -> f32 {
    1.0 - t.clamp(0.0, 1.0)
}

/// An agent that exited while the window watched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitNote {
    pub agent_id: u64,
    pub program: String,
    pub code: i32,
}

impl ExitNote {
    /// The toast's text.
    pub fn text(&self) -> String {
        format!("{} exited with code {}", self.program, self.code)
    }

    /// It ended well: the celebration's cue (when it is turned on).
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// Agents running in `before` and exited in `after`. An agent first seen
/// already exited is not news; nor is a lost one (its daemon went away,
/// a different toast's business).
pub fn newly_exited(before: &[AgentRow], after: &[AgentRow]) -> Vec<ExitNote> {
    after
        .iter()
        .filter(|r| r.info.state() == AgentState::Exited)
        .filter(|r| {
            before
                .iter()
                .any(|b| b.info.id == r.info.id && b.info.state() == AgentState::Running)
        })
        .map(|r| ExitNote {
            agent_id: r.info.id,
            program: program_name(&r.info.program),
            code: r.info.exit_code.unwrap_or(-1),
        })
        .collect()
}

/// The celebration plays: it is turned on and the agent succeeded.
pub fn celebrates(enabled: bool, note: &ExitNote) -> bool {
    enabled && note.success()
}

/// The exit toasts on screen, oldest first, each with when it showed.
#[derive(Clone, Debug, Default)]
pub struct ExitToasts {
    items: Vec<(ExitNote, Instant)>,
}

impl ExitToasts {
    pub fn items(&self) -> impl Iterator<Item = &ExitNote> {
        self.items.iter().map(|(n, _)| n)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Shows `note` (replacing one for the same agent), keeping at most
    /// [`MAX_EXIT_TOASTS`].
    pub fn push(&mut self, note: ExitNote, now: Instant) {
        self.items.retain(|(n, _)| n.agent_id != note.agent_id);
        self.items.push((note, now));
        let extra = self.items.len().saturating_sub(MAX_EXIT_TOASTS);
        self.items.drain(..extra);
    }

    /// Dismisses the toast for `agent_id`; true if one was shown.
    pub fn dismiss(&mut self, agent_id: u64) -> bool {
        let n = self.items.len();
        self.items.retain(|(t, _)| t.agent_id != agent_id);
        self.items.len() != n
    }

    /// Drops toasts older than [`EXIT_TOAST_TTL`], and those whose agent
    /// is no longer listed as exited (restarted or removed meanwhile).
    /// True if any went.
    pub fn prune(&mut self, agents: &[AgentRow], now: Instant) -> bool {
        let n = self.items.len();
        self.items.retain(|(t, at)| {
            now.saturating_duration_since(*at) < EXIT_TOAST_TTL
                && agents
                    .iter()
                    .any(|r| r.info.id == t.agent_id && r.info.state() == AgentState::Exited)
        });
        self.items.len() != n
    }

    /// When the oldest toast goes on its own, if any is shown.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.items.iter().map(|(_, at)| *at + EXIT_TOAST_TTL).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn row(id: u64, state: AgentState, code: Option<i32>) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "/usr/bin/claude".into(),
            cwd: "/w".into(),
            exit_code: code,
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
    fn tween_runs_for_its_length_then_settles() {
        let t0 = Instant::now();
        let t = Tween::new(t0, 100);
        assert_eq!(t.progress(t0), 0.0);
        assert!(t.running(t0));
        assert!((t.progress(t0 + ms(50)) - 0.5).abs() < 1e-3);
        assert!(t.value(t0 + ms(50)) > 0.5, "eased out");
        assert_eq!(t.progress(t0 + ms(100)), 1.0);
        assert!(!t.running(t0 + ms(100)), "no frames once done");
        assert_eq!(t.value(t0 + ms(500)), 1.0);
        // A clock before the start reads as the start.
        assert_eq!(Tween::new(t0 + ms(10), 100).progress(t0), 0.0);
        assert!(!Tween::new(t0, 0).running(t0));
    }

    #[test]
    fn only_new_panes_ease_in_and_not_on_the_first_frame() {
        let (a, b, c) = (PaneId(1), PaneId(2), PaneId(3));
        assert!(entering(&[], &[a, b]).is_empty());
        assert_eq!(entering(&[a], &[a, b]), vec![b]);
        assert_eq!(entering(&[a, b], &[a, c, b]), vec![c]);
        assert!(entering(&[a, b], &[a]).is_empty(), "closing eases nothing");
        assert!(entering(&[a, b], &[b, a]).is_empty());
    }

    #[test]
    fn focus_ring_cross_fades() {
        assert_eq!(ring(true, None), 1.0);
        assert_eq!(ring(false, None), 0.0);
        assert_eq!(ring(true, Some(0.0)), 0.0);
        assert_eq!(ring(false, Some(0.0)), 1.0, "the old pane starts lit");
        assert_eq!(ring(true, Some(0.25)) + ring(false, Some(0.25)), 1.0);
        assert_eq!(ring(true, Some(1.0)), 1.0);
    }

    #[test]
    fn focus_mode_dims_unfocused_panes_only_beside_another() {
        assert!(!dims(false, 3));
        assert!(!dims(true, 1), "a lone (or maximized) pane is not dimmed");
        assert!(dims(true, 2));
        assert_eq!(dim_alpha(true, 1.0), 0.0, "the focused pane is clear");
        assert_eq!(dim_alpha(true, 0.0), DIM_ALPHA);
        assert_eq!(dim_alpha(false, 0.0), 0.0);
        assert!((dim_alpha(true, 0.5) - DIM_ALPHA / 2.0).abs() < 1e-6);
    }

    #[test]
    fn copied_flash_holds_then_fades_and_glow_decays() {
        assert_eq!(flash_alpha(0.0), 1.0);
        assert_eq!(flash_alpha(0.5), 1.0);
        assert!(flash_alpha(0.9) < 0.5);
        assert_eq!(flash_alpha(1.0), 0.0);
        assert_eq!(glow(0.0), 1.0);
        assert_eq!(glow(1.0), 0.0);
        assert_eq!(glow(2.0), 0.0);
    }

    #[test]
    fn running_to_exited_is_news_with_its_code() {
        let before = vec![
            row(1, AgentState::Running, None),
            row(2, AgentState::Running, None),
            row(3, AgentState::Exited, Some(1)),
            row(4, AgentState::Running, None),
        ];
        let after = vec![
            row(1, AgentState::Exited, Some(0)),
            row(2, AgentState::Running, None),
            row(3, AgentState::Exited, Some(1)),
            row(4, AgentState::Lost, None),
            row(5, AgentState::Exited, Some(2)),
        ];
        let notes = newly_exited(&before, &after);
        assert_eq!(
            notes,
            vec![ExitNote {
                agent_id: 1,
                program: "claude".into(),
                code: 0
            }]
        );
        assert_eq!(notes[0].text(), "claude exited with code 0");
        assert!(newly_exited(&[], &after).is_empty(), "the first list");
    }

    #[test]
    fn celebration_needs_the_setting_and_code_zero() {
        let note = |code| ExitNote {
            agent_id: 1,
            program: "x".into(),
            code,
        };
        assert!(celebrates(true, &note(0)));
        assert!(!celebrates(false, &note(0)));
        assert!(!celebrates(true, &note(1)));
        assert!(!celebrates(true, &note(-1)));
    }

    #[test]
    fn exit_toasts_replace_cap_expire_and_follow_the_list() {
        let t0 = Instant::now();
        let note = |id, code| ExitNote {
            agent_id: id,
            program: "p".into(),
            code,
        };
        let mut t = ExitToasts::default();
        assert_eq!(t.next_expiry(), None);
        t.push(note(1, 0), t0);
        t.push(note(1, 3), t0 + ms(10));
        assert_eq!(t.items().count(), 1, "one per agent");
        assert_eq!(t.items().next().unwrap().code, 3);
        for id in 2..=4 {
            t.push(note(id, 0), t0 + ms(20));
        }
        let ids: Vec<u64> = t.items().map(|n| n.agent_id).collect();
        assert_eq!(ids, vec![2, 3, 4], "the oldest went");
        assert_eq!(t.next_expiry(), Some(t0 + ms(20) + EXIT_TOAST_TTL));

        let listed: Vec<AgentRow> = (2..=4)
            .map(|id| row(id, AgentState::Exited, Some(0)))
            .collect();
        assert!(!t.prune(&listed, t0 + ms(30)));
        // Agent 3 was restarted (removed from the list).
        let listed = vec![listed[0].clone(), listed[2].clone()];
        assert!(t.prune(&listed, t0 + ms(30)));
        assert_eq!(t.items().count(), 2);
        assert!(t.dismiss(2));
        assert!(!t.dismiss(2));
        assert!(t.prune(&listed, t0 + ms(20) + EXIT_TOAST_TTL));
        assert!(t.is_empty());
    }
}
