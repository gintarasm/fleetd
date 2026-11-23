//! Pane reconciliation (issue #144): which agent the layout names for
//! each pane, as spawns land, agents restart or exit, and lists arrive.
//!
//! [`PaneSync`] keeps what is on its way into each pane (a spawn, a
//! restart, an attach the user asked for) and, given each pane's layout
//! agent and what its grid shows ([`PaneSnap`]), returns [`Action`]s for
//! the shell to apply in order. It touches neither the layout nor the
//! session itself.

use std::collections::{HashMap, HashSet};

use super::pane::PaneState;
use super::session::PaneId;

/// One pane as the shell sees it before a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSnap {
    pub pane: PaneId,
    /// The agent the layout names for the pane.
    pub layout: Option<u64>,
    /// The pane's grid: its state and the agent it shows; `None` when the
    /// pane has no view.
    pub view: Option<(PaneState, Option<u64>)>,
}

impl PaneSnap {
    /// The grid shows `agent` (in any state).
    fn shows(&self, agent: u64) -> bool {
        self.view.is_some_and(|(_, shown)| shown == Some(agent))
    }
}

/// What the shell does, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Every pane, in any tab, whose layout names `old` names `new`
    /// instead.
    Replace { old: u64, new: Option<u64> },
    /// The layout names `agent` for `pane`, which shows it already.
    Show(PaneId, u64),
    /// `pane`'s connection attaches to `agent`.
    Attach(PaneId, u64),
    /// The layout names no agent for `pane`, which opens empty.
    Empty(PaneId),
    /// A spawn landed: its directory and command line become recent.
    RecordRecent {
        dir: Option<String>,
        command: Option<String>,
    },
}

/// The actions change the layout.
pub fn changes_layout(actions: &[Action]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            Action::Replace { .. } | Action::Show(..) | Action::Empty(_)
        )
    })
}

/// What is on its way into each pane, and which layout agents the latest
/// lists left out.
#[derive(Debug, Default)]
pub struct PaneSync {
    /// Panes a spawn is on its way into (for the "starting…" text only).
    starting: HashSet<PaneId>,
    /// The directory of the spawn on its way into a pane; it becomes a
    /// recent directory once the pane shows the new agent.
    spawn_dirs: HashMap<PaneId, String>,
    /// The New agent sheet's command line of the spawn on its way into a
    /// pane; it becomes a recent command with the directory (ADR 0084).
    spawn_commands: HashMap<PaneId, String>,
    /// Agents asked for in a pane (sidebar click) that it does not show
    /// yet; until then the pane's old agent is not adopted back.
    requested: HashMap<PaneId, u64>,
    /// Panes restarting the agent they show (by its old id) on their own
    /// connection: they show the new agent without attaching to it.
    restarting: HashMap<PaneId, u64>,
    /// Agents in the layout the latest list left out, with the number
    /// (`SessionApi::lists`) of the first list in a row that did.
    unlisted: HashMap<u64, u64>,
    /// `SessionApi::lists` when the layout last followed the list.
    last_list: u64,
}

impl PaneSync {
    /// A spawn is on its way into `pane`, run in `dir`.
    pub fn spawn_started(&mut self, pane: PaneId, dir: String) {
        self.forget(pane);
        self.starting.insert(pane);
        self.spawn_dirs.insert(pane, dir);
    }

    /// The spawn on its way into `pane` came from the New agent sheet's
    /// `command` line.
    pub fn spawn_command(&mut self, pane: PaneId, command: String) {
        self.spawn_commands.insert(pane, command);
    }

    /// `pane` restarts `agent`, which it shows, on its own connection.
    pub fn restart_started(&mut self, pane: PaneId, agent: u64) {
        self.forget(pane);
        self.restarting.insert(pane, agent);
    }

    /// The user asked for `agent` in `pane` (the caller attaches it).
    pub fn request(&mut self, pane: PaneId, agent: u64) {
        self.forget(pane);
        self.requested.insert(pane, agent);
    }

    /// Drops what was on its way into `pane`.
    pub fn forget(&mut self, pane: PaneId) {
        self.starting.remove(&pane);
        self.spawn_dirs.remove(&pane);
        self.spawn_commands.remove(&pane);
        self.requested.remove(&pane);
        self.restarting.remove(&pane);
    }

    /// An error came back, perhaps for a spawn or restart on its way: no
    /// pane says "starting…" any more. Should a spawn land after all, its
    /// pane still adopts its agent; a pane whose restart failed attaches
    /// to a successor it is told of.
    pub fn launch_failed(&mut self) {
        self.starting.clear();
        self.restarting.clear();
    }

    /// A spawn is on its way into `pane`.
    pub fn is_starting(&self, pane: PaneId) -> bool {
        self.starting.contains(&pane)
    }

    /// `pane`'s grid shows `shown` (`None`: nothing yet): the layout
    /// follows it when it changed under it (a spawn landed), except while
    /// an attach the user asked for is on its way.
    pub fn adopt(&mut self, pane: PaneId, layout: Option<u64>, shown: Option<u64>) -> Vec<Action> {
        let Some(shown) = shown else {
            return Vec::new();
        };
        if let Some(&want) = self.requested.get(&pane) {
            if want != shown {
                return Vec::new();
            }
            self.requested.remove(&pane);
        }
        if layout == Some(shown) {
            return Vec::new();
        }
        self.starting.remove(&pane);
        self.restarting.remove(&pane);
        let mut actions = vec![Action::Show(pane, shown)];
        let dir = self.spawn_dirs.remove(&pane);
        let command = self.spawn_commands.remove(&pane);
        if dir.is_some() || command.is_some() {
            actions.push(Action::RecordRecent { dir, command });
        }
        actions
    }

    /// `old` was removed, restarted as `new` if set: every pane whose
    /// layout names it shows `new` (attaching unless it already shows
    /// `new` or started it itself) or becomes empty. `panes` is every
    /// pane, in any tab.
    pub fn follow_removed(
        &mut self,
        old: u64,
        new: Option<u64>,
        panes: &[PaneSnap],
    ) -> Vec<Action> {
        let mut panes = panes.to_vec();
        let mut actions = Vec::new();
        self.removed(old, new, &mut panes, &mut actions);
        actions
    }

    /// [`PaneSync::follow_removed`] on `panes`, whose layout agents follow.
    fn removed(
        &mut self,
        old: u64,
        new: Option<u64>,
        panes: &mut [PaneSnap],
        out: &mut Vec<Action>,
    ) {
        let mut hit = false;
        for snap in panes.iter_mut().filter(|s| s.layout == Some(old)) {
            if !hit {
                out.push(Action::Replace { old, new });
                hit = true;
            }
            snap.layout = new;
            let pane = snap.pane;
            let restarted_here = self.restarting.get(&pane) == Some(&old);
            self.forget(pane);
            match new {
                Some(new) if snap.shows(new) => {}
                Some(new) => {
                    if !restarted_here {
                        out.push(Action::Attach(pane, new));
                    }
                    // Until the pane shows `new`, `old` is not adopted back.
                    self.requested.insert(pane, new);
                }
                None => out.push(Action::Empty(pane)),
            }
        }
    }

    /// `pane`'s connection dropped before its spawn or restart was
    /// answered: it shows `layout`, the agent the layout names (the
    /// successor, if a restart was announced meanwhile; else the old
    /// agent, until one is).
    pub fn launch_dropped(&mut self, pane: PaneId, layout: Option<u64>) -> Vec<Action> {
        self.forget(pane);
        match layout {
            Some(agent) => {
                self.requested.insert(pane, agent);
                vec![Action::Attach(pane, agent)]
            }
            None => Vec::new(),
        }
    }

    /// After list number `list` (`SessionApi::lists`), which lists the
    /// agents `listed` says and the restarts `replaced` (old → new):
    /// panes follow restarts they missed, and a pane whose agent the node
    /// neither lists nor replaced becomes empty, unless a spawn, restart
    /// or attach is on its way into it. A lost pane is emptied at once;
    /// an exited one (its final screen) only once two lists in a row left
    /// its agent out, since an agent spawned after the first was taken
    /// may have exited already. `panes` is every pane, in any tab.
    pub fn follow_list(
        &mut self,
        panes: &[PaneSnap],
        listed: impl Fn(u64) -> bool,
        replaced: &HashMap<u64, u64>,
        list: u64,
    ) -> Vec<Action> {
        let fresh = std::mem::replace(&mut self.last_list, list) != list;
        let mut panes = panes.to_vec();
        let mut unlisted = HashMap::new();
        let mut actions = Vec::new();
        for i in 0..panes.len() {
            let snap = panes[i];
            let pane = snap.pane;
            let Some(id) = snap.layout else {
                continue;
            };
            if let Some(&new) = replaced.get(&id) {
                self.removed(id, Some(new), &mut panes, &mut actions);
                continue;
            }
            if listed(id) {
                continue;
            }
            // `requested` names the layout's agent, so a request for `id`
            // itself does not count: its failed attach is what `Lost` says.
            let on_its_way = self.starting.contains(&pane)
                || self.restarting.contains_key(&pane)
                || self.requested.get(&pane).is_some_and(|&want| want != id);
            if on_its_way {
                continue;
            }
            let Some((state, shown)) = snap.view else {
                continue;
            };
            let gone = match state {
                PaneState::Lost => true,
                PaneState::Exited(_) if shown == Some(id) => {
                    // Counted from a list, not from events applied to an
                    // older one (which predates agents spawned since).
                    let first = match self.unlisted.get(&id) {
                        Some(&n) => n,
                        None if fresh => list,
                        None => continue,
                    };
                    unlisted.insert(id, first);
                    first < list
                }
                _ => false,
            };
            if gone {
                panes[i].layout = None;
                self.forget(pane);
                actions.push(Action::Empty(pane));
            }
        }
        self.unlisted = unlisted;
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P1: PaneId = PaneId(1);
    const P2: PaneId = PaneId(2);

    fn snap(pane: PaneId, layout: Option<u64>, state: PaneState, shown: Option<u64>) -> PaneSnap {
        PaneSnap {
            pane,
            layout,
            view: Some((state, shown)),
        }
    }

    fn none_listed(_: u64) -> bool {
        false
    }

    // adopt

    #[test]
    fn adopt_ignores_a_pane_showing_nothing() {
        let mut s = PaneSync::default();
        assert_eq!(s.adopt(P1, Some(1), None), vec![]);
    }

    #[test]
    fn adopt_does_nothing_when_the_layout_already_names_the_agent() {
        let mut s = PaneSync::default();
        assert_eq!(s.adopt(P1, Some(1), Some(1)), vec![]);
    }

    #[test]
    fn a_landed_spawn_is_adopted_and_recorded_as_recent() {
        let mut s = PaneSync::default();
        s.spawn_started(P1, "~/src".into());
        s.spawn_command(P1, "claude".into());
        assert!(s.is_starting(P1));
        let actions = s.adopt(P1, None, Some(7));
        assert_eq!(
            actions,
            vec![
                Action::Show(P1, 7),
                Action::RecordRecent {
                    dir: Some("~/src".into()),
                    command: Some("claude".into()),
                },
            ]
        );
        assert!(changes_layout(&actions));
        assert!(!s.is_starting(P1));
        // Recorded once only.
        assert_eq!(s.adopt(P1, None, Some(8)), vec![Action::Show(P1, 8)]);
    }

    #[test]
    fn adopt_waits_for_the_requested_agent() {
        let mut s = PaneSync::default();
        s.request(P1, 5);
        // The old agent still on screen is not adopted back.
        assert_eq!(s.adopt(P1, Some(5), Some(4)), vec![]);
        // Once shown, the request is done: the layout names it already.
        assert_eq!(s.adopt(P1, Some(5), Some(5)), vec![]);
        assert_eq!(s.adopt(P1, Some(5), Some(4)), vec![Action::Show(P1, 4)]);
    }

    #[test]
    fn adopt_ends_a_restart_in_the_pane() {
        let mut s = PaneSync::default();
        s.restart_started(P1, 3);
        assert_eq!(s.adopt(P1, Some(3), Some(9)), vec![Action::Show(P1, 9)]);
        // No longer restarting: a removal of 3 now attaches.
        let panes = [snap(P1, Some(3), PaneState::Live, Some(9))];
        assert!(
            s.follow_removed(3, Some(10), &panes)
                .contains(&Action::Attach(P1, 10))
        );
    }

    // follow_removed

    #[test]
    fn a_removed_agent_with_no_successor_empties_its_panes() {
        let mut s = PaneSync::default();
        let panes = [
            snap(P1, Some(1), PaneState::Exited(0), Some(1)),
            snap(P2, Some(2), PaneState::Live, Some(2)),
        ];
        let actions = s.follow_removed(1, None, &panes);
        assert_eq!(
            actions,
            vec![Action::Replace { old: 1, new: None }, Action::Empty(P1)]
        );
    }

    #[test]
    fn a_removal_no_pane_shows_changes_nothing() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(2), PaneState::Live, Some(2))];
        let actions = s.follow_removed(1, Some(3), &panes);
        assert_eq!(actions, vec![]);
        assert!(!changes_layout(&actions));
    }

    #[test]
    fn a_restarted_agent_is_attached_in_every_pane_showing_it() {
        let mut s = PaneSync::default();
        let panes = [
            snap(P1, Some(1), PaneState::Exited(0), Some(1)),
            snap(P2, Some(1), PaneState::Live, Some(1)),
        ];
        assert_eq!(
            s.follow_removed(1, Some(2), &panes),
            vec![
                Action::Replace {
                    old: 1,
                    new: Some(2)
                },
                Action::Attach(P1, 2),
                Action::Attach(P2, 2),
            ]
        );
        // Until it shows 2, the old agent is not adopted back.
        assert_eq!(s.adopt(P1, Some(2), Some(1)), vec![]);
    }

    #[test]
    fn a_pane_already_showing_the_successor_is_not_attached() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Live, Some(2))];
        assert_eq!(
            s.follow_removed(1, Some(2), &panes),
            vec![Action::Replace {
                old: 1,
                new: Some(2)
            }]
        );
        // Nothing requested: the pane adopts whatever it shows next.
        assert_eq!(s.adopt(P1, Some(2), Some(4)), vec![Action::Show(P1, 4)]);
    }

    #[test]
    fn a_pane_restarting_the_agent_itself_is_not_attached() {
        let mut s = PaneSync::default();
        s.restart_started(P1, 1);
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert_eq!(
            s.follow_removed(1, Some(2), &panes),
            vec![Action::Replace {
                old: 1,
                new: Some(2)
            }]
        );
        // But it waits for the successor.
        assert_eq!(s.adopt(P1, Some(2), Some(1)), vec![]);
    }

    #[test]
    fn a_restart_of_another_agent_still_attaches() {
        let mut s = PaneSync::default();
        s.restart_started(P1, 9);
        let panes = [snap(P1, Some(1), PaneState::Live, Some(1))];
        assert!(
            s.follow_removed(1, Some(2), &panes)
                .contains(&Action::Attach(P1, 2))
        );
    }

    #[test]
    fn a_pane_without_a_view_is_attached() {
        let mut s = PaneSync::default();
        let panes = [PaneSnap {
            pane: P1,
            layout: Some(1),
            view: None,
        }];
        assert!(
            s.follow_removed(1, Some(2), &panes)
                .contains(&Action::Attach(P1, 2))
        );
    }

    #[test]
    fn a_removal_drops_the_spawn_on_its_way() {
        let mut s = PaneSync::default();
        s.spawn_started(P1, "/d".into());
        let panes = [snap(P1, Some(1), PaneState::Live, Some(1))];
        s.follow_removed(1, None, &panes);
        assert!(!s.is_starting(P1));
        assert_eq!(s.adopt(P1, None, Some(5)), vec![Action::Show(P1, 5)]);
    }

    // launch_dropped

    #[test]
    fn a_dropped_launch_attaches_the_layout_agent() {
        let mut s = PaneSync::default();
        s.spawn_started(P1, "/d".into());
        assert_eq!(s.launch_dropped(P1, Some(4)), vec![Action::Attach(P1, 4)]);
        assert!(!s.is_starting(P1));
        // The old agent is wanted: what the pane shows meanwhile is not.
        assert_eq!(s.adopt(P1, Some(4), Some(6)), vec![]);
    }

    #[test]
    fn a_dropped_launch_in_an_empty_pane_only_forgets() {
        let mut s = PaneSync::default();
        s.spawn_started(P1, "/d".into());
        assert_eq!(s.launch_dropped(P1, None), vec![]);
        assert!(!s.is_starting(P1));
    }

    // launch_failed

    #[test]
    fn a_failed_launch_keeps_the_directory_for_a_late_spawn() {
        let mut s = PaneSync::default();
        s.spawn_started(P1, "/d".into());
        s.launch_failed();
        assert!(!s.is_starting(P1));
        assert_eq!(
            s.adopt(P1, None, Some(3)),
            vec![
                Action::Show(P1, 3),
                Action::RecordRecent {
                    dir: Some("/d".into()),
                    command: None
                },
            ]
        );
    }

    #[test]
    fn a_failed_launch_ends_a_pane_restart() {
        let mut s = PaneSync::default();
        s.restart_started(P1, 1);
        s.launch_failed();
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert!(
            s.follow_removed(1, Some(2), &panes)
                .contains(&Action::Attach(P1, 2))
        );
    }

    // follow_list

    #[test]
    fn a_listed_agent_stays() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Lost, Some(1))];
        assert_eq!(
            s.follow_list(&panes, |id| id == 1, &HashMap::new(), 1),
            vec![]
        );
    }

    #[test]
    fn empty_panes_are_skipped() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, None, PaneState::Lost, None)];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
    }

    #[test]
    fn a_missed_restart_is_followed() {
        let mut s = PaneSync::default();
        let panes = [
            snap(P1, Some(1), PaneState::Exited(0), Some(1)),
            snap(P2, Some(1), PaneState::Exited(0), Some(1)),
        ];
        let replaced = HashMap::from([(1, 2)]);
        // P2 follows with P1; then names 2, which is listed.
        assert_eq!(
            s.follow_list(&panes, |id| id == 2, &replaced, 1),
            vec![
                Action::Replace {
                    old: 1,
                    new: Some(2)
                },
                Action::Attach(P1, 2),
                Action::Attach(P2, 2),
            ]
        );
    }

    #[test]
    fn a_followed_restart_unlisted_too_empties_the_later_lost_pane() {
        let mut s = PaneSync::default();
        let panes = [
            snap(P1, Some(1), PaneState::Lost, Some(1)),
            snap(P2, Some(1), PaneState::Lost, Some(1)),
        ];
        let replaced = HashMap::from([(1, 2)]);
        // P2 now names 2 and has asked for it: a request for the layout's
        // own agent does not count, and its grid is Lost.
        let actions = s.follow_list(&panes, none_listed, &replaced, 1);
        assert_eq!(
            actions,
            vec![
                Action::Replace {
                    old: 1,
                    new: Some(2)
                },
                Action::Attach(P1, 2),
                Action::Attach(P2, 2),
                Action::Empty(P2),
            ]
        );
    }

    #[test]
    fn a_lost_pane_is_emptied_at_once() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Lost, Some(1))];
        let actions = s.follow_list(&panes, none_listed, &HashMap::new(), 1);
        assert_eq!(actions, vec![Action::Empty(P1)]);
        assert!(changes_layout(&actions));
    }

    #[test]
    fn live_attaching_and_disconnected_panes_stay() {
        for state in [
            PaneState::Live,
            PaneState::Attaching,
            PaneState::Disconnected,
        ] {
            let mut s = PaneSync::default();
            let panes = [snap(P1, Some(1), state, Some(1))];
            for list in 1..=3 {
                assert_eq!(
                    s.follow_list(&panes, none_listed, &HashMap::new(), list),
                    vec![]
                );
            }
        }
    }

    #[test]
    fn a_pane_without_a_view_stays() {
        let mut s = PaneSync::default();
        let panes = [PaneSnap {
            pane: P1,
            layout: Some(1),
            view: None,
        }];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
    }

    #[test]
    fn an_exited_pane_is_emptied_after_two_lists_in_a_row() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 2),
            vec![Action::Empty(P1)]
        );
    }

    #[test]
    fn events_on_the_same_list_do_not_count_as_a_second_list() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 2),
            vec![Action::Empty(P1)]
        );
    }

    #[test]
    fn the_count_starts_from_a_fresh_list_only() {
        let mut s = PaneSync::default();
        let p1 = [snap(P1, Some(1), PaneState::Live, Some(1))];
        // List 1 seen while the agent was live.
        assert_eq!(s.follow_list(&p1, |id| id == 1, &HashMap::new(), 1), vec![]);
        // Events applied to list 1: the agent is gone and has exited, but
        // list 1 is not fresh, so the count does not start.
        let p1 = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert_eq!(s.follow_list(&p1, none_listed, &HashMap::new(), 1), vec![]);
        // List 2 is the first to count; list 3 the second.
        assert_eq!(s.follow_list(&p1, none_listed, &HashMap::new(), 2), vec![]);
        assert_eq!(
            s.follow_list(&p1, none_listed, &HashMap::new(), 3),
            vec![Action::Empty(P1)]
        );
    }

    #[test]
    fn a_relisted_agent_restarts_the_count() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(1))];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, |id| id == 1, &HashMap::new(), 2),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 3),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 4),
            vec![Action::Empty(P1)]
        );
    }

    #[test]
    fn an_exited_pane_showing_another_agent_stays() {
        let mut s = PaneSync::default();
        let panes = [snap(P1, Some(1), PaneState::Exited(0), Some(2))];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 2),
            vec![]
        );
    }

    #[test]
    fn panes_with_something_on_its_way_stay() {
        let lost = [snap(P1, Some(1), PaneState::Lost, Some(1))];
        let mut s = PaneSync::default();
        s.spawn_started(P1, "/d".into());
        assert_eq!(
            s.follow_list(&lost, none_listed, &HashMap::new(), 1),
            vec![]
        );

        let mut s = PaneSync::default();
        s.restart_started(P1, 1);
        assert_eq!(
            s.follow_list(&lost, none_listed, &HashMap::new(), 1),
            vec![]
        );

        let mut s = PaneSync::default();
        s.request(P1, 2);
        assert_eq!(
            s.follow_list(&lost, none_listed, &HashMap::new(), 1),
            vec![]
        );
    }

    #[test]
    fn a_request_for_the_layout_agent_itself_does_not_keep_the_pane() {
        let mut s = PaneSync::default();
        s.request(P1, 1);
        let panes = [snap(P1, Some(1), PaneState::Lost, Some(1))];
        assert_eq!(
            s.follow_list(&panes, none_listed, &HashMap::new(), 1),
            vec![Action::Empty(P1)]
        );
    }

    #[test]
    fn an_emptied_pane_forgets_what_was_on_its_way() {
        let mut s = PaneSync::default();
        s.request(P1, 1);
        s.spawn_command(P1, "cmd".into());
        let panes = [snap(P1, Some(1), PaneState::Lost, Some(1))];
        s.follow_list(&panes, none_listed, &HashMap::new(), 1);
        // The command is gone, and no request holds adoption back.
        assert_eq!(s.adopt(P1, None, Some(9)), vec![Action::Show(P1, 9)]);
    }

    #[test]
    fn the_count_is_per_agent_and_dropped_once_listed_away() {
        let mut s = PaneSync::default();
        let both = [
            snap(P1, Some(1), PaneState::Exited(0), Some(1)),
            snap(P2, Some(2), PaneState::Exited(0), Some(2)),
        ];
        // List 1 leaves out 1 only; list 2 leaves out both.
        assert_eq!(
            s.follow_list(&both, |id| id == 2, &HashMap::new(), 1),
            vec![]
        );
        assert_eq!(
            s.follow_list(&both, none_listed, &HashMap::new(), 2),
            vec![Action::Empty(P1)]
        );
        assert_eq!(
            s.follow_list(&both[1..], none_listed, &HashMap::new(), 3),
            vec![Action::Empty(P2)]
        );
    }
}
