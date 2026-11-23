//! "Next agent that needs you" (ADR 0100): which agent ⌘J goes to.
//! Agents that need you come first, the one waiting longest first; failed
//! agents follow, the one that failed longest ago first. Repeating the
//! action walks the list and wraps.

use std::collections::HashMap;

use super::status::AgentStatus;

/// Since when each agent has had its current status, as far as this GUI
/// has watched (the node does not say when a status began).
#[derive(Debug, Default)]
pub struct Since {
    seen: HashMap<u64, (AgentStatus, i64)>,
}

impl Since {
    /// Learns `now`'s statuses at `now_ms`: an agent whose status changed
    /// (or is new) starts again from `now_ms`; gone agents are forgotten.
    pub fn update(&mut self, now: &HashMap<u64, AgentStatus>, now_ms: i64) {
        self.seen.retain(|id, _| now.contains_key(id));
        for (id, status) in now {
            match self.seen.get(id) {
                Some((s, _)) if s == status => {}
                _ => {
                    self.seen.insert(*id, (*status, now_ms));
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }

    /// The agents to go to, in order: needs-you, then failed; each
    /// oldest first (ties by id).
    pub fn queue(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for wanted in [AgentStatus::NeedsYou, AgentStatus::Failed] {
            let mut group: Vec<(i64, u64)> = self
                .seen
                .iter()
                .filter(|(_, (s, _))| *s == wanted)
                .map(|(id, (_, at))| (*at, *id))
                .collect();
            group.sort_unstable();
            out.extend(group.into_iter().map(|(_, id)| id));
        }
        out
    }
}

/// The agent after `current` in `queue`, wrapping; the first when
/// `current` is not in it. `None` for an empty queue.
pub fn next_after(queue: &[u64], current: Option<u64>) -> Option<u64> {
    let first = *queue.first()?;
    let Some(i) = current.and_then(|c| queue.iter().position(|&id| id == c)) else {
        return Some(first);
    };
    Some(queue[(i + 1) % queue.len()])
}

#[cfg(test)]
mod tests {
    use super::AgentStatus::{Done, Failed, NeedsYou, Working};
    use super::*;

    fn st(list: &[(u64, AgentStatus)]) -> HashMap<u64, AgentStatus> {
        list.iter().copied().collect()
    }

    #[test]
    fn needs_you_oldest_first_then_failed() {
        let mut s = Since::default();
        s.update(&st(&[(1, Working), (2, Failed), (3, Working)]), 100);
        s.update(&st(&[(1, NeedsYou), (2, Failed), (3, Working)]), 200);
        s.update(&st(&[(1, NeedsYou), (2, Failed), (3, NeedsYou)]), 300);
        assert_eq!(s.queue(), vec![1, 3, 2]);
        // Agent 3 started waiting first in a new run: order follows time.
        let mut s = Since::default();
        s.update(&st(&[(1, Working), (3, NeedsYou)]), 100);
        s.update(&st(&[(1, NeedsYou), (3, NeedsYou)]), 200);
        assert_eq!(s.queue(), vec![3, 1]);
    }

    #[test]
    fn a_status_change_restarts_the_wait() {
        let mut s = Since::default();
        s.update(&st(&[(1, NeedsYou), (2, NeedsYou)]), 100);
        s.update(&st(&[(1, Working), (2, NeedsYou)]), 200);
        s.update(&st(&[(1, NeedsYou), (2, NeedsYou)]), 300);
        assert_eq!(s.queue(), vec![2, 1]);
    }

    #[test]
    fn ties_go_by_id_and_gone_or_done_agents_drop_out() {
        let mut s = Since::default();
        s.update(&st(&[(5, NeedsYou), (2, NeedsYou), (9, Done)]), 100);
        assert_eq!(s.queue(), vec![2, 5]);
        s.update(&st(&[(5, NeedsYou)]), 200);
        assert_eq!(s.queue(), vec![5]);
        s.clear();
        assert!(s.queue().is_empty());
    }

    #[test]
    fn next_after_cycles_and_wraps() {
        let q = [4, 7, 9];
        assert_eq!(next_after(&q, None), Some(4));
        assert_eq!(next_after(&q, Some(1)), Some(4));
        assert_eq!(next_after(&q, Some(4)), Some(7));
        assert_eq!(next_after(&q, Some(9)), Some(4));
        assert_eq!(next_after(&[], Some(4)), None);
    }
}
