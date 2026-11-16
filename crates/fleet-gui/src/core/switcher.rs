//! The agent switcher (⌃Tab): the agents in most-recently-used order as
//! [`PickerItem`]s, and which row a press of ⌃Tab or ⌃⇧Tab starts on.
//! The shell lists them with the shared picker and shows the chosen one
//! in the focused pane when ⌃ is released.

use std::collections::VecDeque;
use std::path::Path;

use super::palette::{Entry, agent_item};
use super::picker::{PickerItem, RECENTS_CAP, Recents};
use super::session::AgentRow;

/// The agents shown in the focused pane, most recent first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentMru(VecDeque<u64>);

impl AgentMru {
    /// The focused pane shows `agent` now: it becomes the most recent.
    /// Cheap when nothing changed (called each frame).
    pub fn observe(&mut self, agent: Option<u64>) {
        let Some(id) = agent else {
            return;
        };
        if self.0.front() == Some(&id) {
            return;
        }
        self.0.retain(|&a| a != id);
        self.0.push_front(id);
        self.0.truncate(RECENTS_CAP);
    }

    /// The most recent agent first.
    pub fn order(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().copied()
    }

    /// As the picker's recent picks (keys `agent:<id>`), so an empty
    /// query lists the agents in this order.
    pub fn recents(&self) -> Recents {
        let mut r = Recents::default();
        for &id in self.0.iter().rev() {
            r.touch(&Entry::Agent(id).key());
        }
        r
    }
}

/// The switcher's rows, newest first (the picker puts the recent ones
/// ahead): the palette's agent row with the id in the detail and the
/// agent's last line as the preview.
pub fn switcher_items(agents: &[AgentRow], home: &Path) -> Vec<(u64, PickerItem)> {
    let mut rows: Vec<&AgentRow> = agents.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    rows.into_iter()
        .map(|r| {
            let id = r.info.id;
            let mut item = agent_item(r, home);
            item.detail = format!("#{id} · {}", item.detail);
            item.preview = r.last_line.trim().to_owned();
            (id, item)
        })
        .collect()
}

/// The row a fresh ⌃Tab (or ⌃⇧Tab, `backward`) selects among `rows`:
/// forward, the one after the current agent (when it is listed first);
/// backward, the last.
pub fn start_row(rows: usize, current_first: bool, backward: bool) -> usize {
    match rows {
        0 => 0,
        n if backward => n - 1,
        n if current_first && n > 1 => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::picker::Picker;
    use fleet_proto::{AgentInfo, AgentState};

    fn row(id: u64, created: i64, last: &str) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "/usr/bin/claude".into(),
            cwd: format!("/Users/me/p{id}"),
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(AgentState::Running);
        AgentRow {
            info,
            last_line: last.into(),
            last_output_unix_ms: None,
        }
    }

    fn home() -> &'static Path {
        Path::new("/Users/me")
    }

    #[test]
    fn the_mru_moves_the_shown_agent_to_the_front() {
        let mut m = AgentMru::default();
        m.observe(Some(1));
        m.observe(Some(2));
        m.observe(None);
        m.observe(Some(2));
        m.observe(Some(3));
        m.observe(Some(1));
        assert_eq!(m.order().collect::<Vec<_>>(), [1, 3, 2]);
        for i in 10..10 + RECENTS_CAP as u64 {
            m.observe(Some(i));
        }
        assert_eq!(m.order().count(), RECENTS_CAP);
        assert!(!m.order().any(|i| i == 1));
    }

    #[test]
    fn rows_show_id_folder_status_and_last_line() {
        let items = switcher_items(&[row(7, 1, "  $ cargo test \n")], home());
        let (id, item) = &items[0];
        assert_eq!(*id, 7);
        assert_eq!(item.key, "agent:7");
        assert_eq!(item.title, "claude in ~/p7");
        assert_eq!(item.detail, "#7 · ~/p7 · running");
        assert_eq!(item.preview, "$ cargo test");
    }

    #[test]
    fn the_picker_lists_them_most_recent_first_then_newest() {
        let agents = [row(1, 1, ""), row(2, 2, ""), row(3, 3, ""), row(4, 4, "")];
        let mut m = AgentMru::default();
        m.observe(Some(1));
        m.observe(Some(3));
        let items = switcher_items(&agents, home())
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        let p = Picker::new(items, m.recents());
        let keys: Vec<_> = p
            .ranked()
            .iter()
            .map(|r| p.items()[r.item].key.as_str())
            .collect();
        assert_eq!(keys, ["agent:3", "agent:1", "agent:4", "agent:2"]);
    }

    #[test]
    fn a_press_starts_after_the_current_agent_or_at_the_end() {
        assert_eq!(start_row(3, true, false), 1);
        assert_eq!(start_row(3, true, true), 2);
        // The focused pane shows no listed agent: the first is the target.
        assert_eq!(start_row(3, false, false), 0);
        assert_eq!(start_row(1, true, false), 0);
        assert_eq!(start_row(1, true, true), 0);
        assert_eq!(start_row(0, false, false), 0);
    }
}
