//! The new-child highlight (ADR 0033, 0079): an agent another agent
//! spawned while the GUI watched has its sidebar row and graph card
//! highlighted briefly, so a spawn never goes unnoticed.
//!
//! Only agents with a parent (`fleet.parent`) count, and only those first
//! listed after the GUI's first list: what was already there when it
//! connected is not news. A restarted agent (a successor in `replaced`) is
//! not a new child.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::session::AgentRow;
use super::sidebar_tree::parent;
use super::style::motion::ease_out;

/// How long the highlight stays at full strength.
pub const HOLD: Duration = Duration::from_millis(1600);
/// How long it then fades (with full motion; reduced motion ends it at once).
pub const FADE: Duration = Duration::from_millis(900);

/// When each new child was first seen.
#[derive(Debug, Default)]
pub struct NewChildren {
    /// Every id the latest list showed.
    seen: HashSet<u64>,
    /// The first list was taken: from then on unseen ids are new.
    primed: bool,
    since: HashMap<u64, Instant>,
}

impl NewChildren {
    /// Takes the latest rows (`replaced`: old id to its successor).
    pub fn observe(&mut self, rows: &[AgentRow], replaced: &HashMap<u64, u64>, now: Instant) {
        let successors: HashSet<u64> = replaced.values().copied().collect();
        if self.primed {
            for row in rows {
                let id = row.info.id;
                if !self.seen.contains(&id) && parent(row).is_some() && !successors.contains(&id) {
                    self.since.insert(id, now);
                }
            }
        }
        self.seen = rows.iter().map(|r| r.info.id).collect();
        self.primed = true;
        let seen = &self.seen;
        self.since
            .retain(|id, at| seen.contains(id) && now.duration_since(*at) < HOLD + FADE);
    }

    /// How strongly `id` is highlighted now, 0 (not at all) to 1.
    pub fn strength(&self, id: u64, now: Instant, reduce_motion: bool) -> f32 {
        let Some(at) = self.since.get(&id) else {
            return 0.0;
        };
        let age = now.saturating_duration_since(*at);
        if age < HOLD {
            return 1.0;
        }
        if reduce_motion {
            return 0.0;
        }
        let t = (age - HOLD).as_secs_f32() / FADE.as_secs_f32();
        1.0 - ease_out(t)
    }

    /// Every highlighted agent's strength now (none at 0).
    pub fn strengths(&self, now: Instant, reduce_motion: bool) -> HashMap<u64, f32> {
        self.since
            .keys()
            .map(|&id| (id, self.strength(id, now, reduce_motion)))
            .filter(|&(_, s)| s > 0.0)
            .collect()
    }

    /// When the next redraw is due for the highlights to change: `None`
    /// when none is shown. While one fades (full motion) that is the next
    /// frame (`Some(ZERO)`); otherwise when the next hold ends.
    pub fn next_change(&self, now: Instant, reduce_motion: bool) -> Option<Duration> {
        self.since
            .values()
            .filter_map(|at| {
                let age = now.saturating_duration_since(*at);
                if age < HOLD {
                    Some(HOLD - age)
                } else if !reduce_motion && age < HOLD + FADE {
                    Some(Duration::ZERO)
                } else {
                    None
                }
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn row(id: u64, parent: Option<u64>) -> AgentRow {
        let mut info = AgentInfo {
            id,
            ..Default::default()
        };
        if let Some(p) = parent {
            info.labels.insert("fleet.parent".into(), p.to_string());
        }
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn only_children_spawned_after_the_first_list_are_new() {
        let t0 = Instant::now();
        let mut n = NewChildren::default();
        let none = HashMap::new();
        n.observe(&[row(1, None), row(2, Some(1))], &none, t0);
        assert!(n.strengths(t0, false).is_empty(), "already there");
        n.observe(
            &[row(1, None), row(2, Some(1)), row(3, Some(1)), row(4, None)],
            &none,
            t0,
        );
        assert_eq!(n.strengths(t0, false), HashMap::from([(3, 1.0)]));
        // A restart's successor is not a new child.
        n.observe(
            &[row(1, None), row(3, Some(1)), row(5, Some(1))],
            &HashMap::from([(2, 5)]),
            t0,
        );
        assert_eq!(n.strength(5, t0, false), 0.0);
        assert_eq!(n.strength(3, t0, false), 1.0);
    }

    #[test]
    fn it_holds_then_fades_or_ends_at_once_under_reduced_motion() {
        let t0 = Instant::now();
        let mut n = NewChildren::default();
        let none = HashMap::new();
        n.observe(&[], &none, t0);
        n.observe(&[row(2, Some(1))], &none, t0);
        assert_eq!(n.strength(2, t0 + HOLD / 2, false), 1.0);
        assert_eq!(n.strength(2, t0 + HOLD / 2, true), 1.0);
        assert_eq!(n.next_change(t0, false), Some(HOLD));
        let fading = t0 + HOLD + FADE / 2;
        let s = n.strength(2, fading, false);
        assert!(s > 0.0 && s < 1.0, "{s}");
        assert_eq!(n.next_change(fading, false), Some(Duration::ZERO));
        assert_eq!(n.strength(2, fading, true), 0.0);
        assert_eq!(n.next_change(fading, true), None);
        let done = t0 + HOLD + FADE;
        assert_eq!(n.strength(2, done, false), 0.0);
        assert_eq!(n.next_change(done, false), None);
        n.observe(&[row(2, Some(1))], &none, done);
        assert!(n.since.is_empty(), "pruned");
    }

    #[test]
    fn a_removed_child_loses_its_highlight() {
        let t0 = Instant::now();
        let mut n = NewChildren::default();
        let none = HashMap::new();
        n.observe(&[], &none, t0);
        n.observe(&[row(2, Some(1))], &none, t0);
        n.observe(&[], &none, t0);
        assert_eq!(n.strength(2, t0, false), 0.0);
    }
}
