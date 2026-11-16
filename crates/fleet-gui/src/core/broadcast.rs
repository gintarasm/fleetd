//! Send to Agents… (issue #95, ADR 0083): type the same text into one or
//! many running agents, as `fleet send` does, with Return after it if
//! asked. What can be picked, the sheet's choices, the warning when a
//! target is working (ADR 0042), and the bytes each agent gets.
//!
//! The sidebar's ⌘-click and ⇧-click selection ([`RowSelection`]) is
//! here too: it is what Send to Selected… starts from.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use fleet_client::names::name_of;
use fleet_proto::AgentState;
use fleet_term::Modes;

use super::keys::encode_paste;
use super::palette::agent_item;
use super::picker::PickerItem;
use super::session::AgentRow;
use super::sidebar::program_and_id;
use super::status::AgentStatus;

/// The pause between the text and its Return, as `fleet send --enter`
/// has it: a TUI reading both at once takes them as one paste and leaves
/// the text typed but unsent.
pub const RETURN_DELAY_MS: u64 = 150;

/// What the Return after the text is.
pub const RETURN: &[u8] = b"\r";

/// The picker key of agent `id` (as the palette's agent rows).
pub fn agent_key(id: u64) -> String {
    format!("agent:{id}")
}

/// The agent a picker key names.
pub fn key_agent(key: &str) -> Option<u64> {
    key.strip_prefix("agent:")?.parse().ok()
}

/// The agent can be typed into: it is running.
pub fn sendable(row: &AgentRow) -> bool {
    row.info.state() == AgentState::Running
}

/// The picker's rows: the running agents, newest first, found by name,
/// folder and labels (`role=reviewer`), each with its id and status.
pub fn send_items(
    agents: &[AgentRow],
    statuses: &HashMap<u64, AgentStatus>,
    home: &Path,
) -> Vec<PickerItem> {
    let mut rows: Vec<&AgentRow> = agents.iter().filter(|r| sendable(r)).collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    rows.into_iter()
        .map(|r| {
            let id = r.info.id;
            let mut item = agent_item(r, home);
            let cwd = item.detail.split(" · ").next().unwrap_or("").to_owned();
            item.detail = match statuses.get(&id) {
                Some(s) => format!("#{id} · {cwd} · {}", s.label()),
                None => format!("#{id} · {cwd}"),
            };
            item
        })
        .collect()
}

/// How a target is named in the sheet: its name, else `program · id`.
pub fn target_name(row: &AgentRow) -> String {
    name_of(&row.info).map_or_else(|| program_and_id(&row.info), str::to_owned)
}

/// The text as sent: trailing line breaks dropped (Return is the
/// checkbox's), the rest as typed.
pub fn message(text: &str) -> &str {
    text.trim_end_matches(['\n', '\r'])
}

/// The bytes an agent gets for `text`, like `fleet send`: as typed. A
/// multi-line text goes as one bracketed paste where the agent asked for
/// bracketed paste (`bracketed`: a pane showing it says so), so its line
/// breaks do not submit it line by line.
pub fn encode_text(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed && text.contains('\n') {
        let modes = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        encode_paste(text, &modes)
    } else {
        text.as_bytes().to_vec()
    }
}

/// What a Send does: these agents get `text` now, then (if
/// `press_return`) a Return [`RETURN_DELAY_MS`] later, or at once when
/// there is no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendPlan {
    pub text: String,
    pub targets: Vec<u64>,
    pub press_return: bool,
}

impl SendPlan {
    /// Wait before the Return: there was text before it.
    pub fn return_delayed(&self) -> bool {
        self.press_return && !self.text.is_empty()
    }
}

/// The answer to Send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submit {
    /// Nothing to send; why.
    Nothing(&'static str),
    /// Some targets are working: this warning is shown, and Send again
    /// sends anyway.
    Warn(String),
    Send(SendPlan),
}

/// The sheet's choices: the targets (in the order picked), Return after
/// the text, and the working targets already warned about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendForm {
    selected: Vec<u64>,
    pub press_return: bool,
    warned: Option<Vec<u64>>,
}

impl SendForm {
    /// `preselect` picked, Return on (the default).
    pub fn new(preselect: impl IntoIterator<Item = u64>) -> Self {
        let mut form = SendForm {
            selected: Vec::new(),
            press_return: true,
            warned: None,
        };
        for id in preselect {
            if !form.selected.contains(&id) {
                form.selected.push(id);
            }
        }
        form
    }

    pub fn selected(&self) -> &[u64] {
        &self.selected
    }

    pub fn is_selected(&self, id: u64) -> bool {
        self.selected.contains(&id)
    }

    /// Picks `id`, or unpicks it; a warning shown is withdrawn.
    pub fn toggle(&mut self, id: u64) {
        if let Some(i) = self.selected.iter().position(|&s| s == id) {
            self.selected.remove(i);
        } else {
            self.selected.push(id);
        }
        self.warned = None;
    }

    pub fn toggle_return(&mut self) {
        self.press_return = !self.press_return;
    }

    /// Drops targets that are no longer running (`sendable` says).
    pub fn retain(&mut self, sendable: impl Fn(u64) -> bool) {
        let before = self.selected.len();
        self.selected.retain(|&id| sendable(id));
        if self.selected.len() != before {
            self.warned = None;
        }
    }

    /// The warning shown, if any (Send now sends anyway).
    pub fn warning_shown(&self) -> bool {
        self.warned.is_some()
    }

    /// Send: `text` to the targets, unless there is nothing to send, or a
    /// target is `working` that no warning has named yet.
    pub fn submit(
        &mut self,
        text: &str,
        working: &HashSet<u64>,
        name: impl Fn(u64) -> String,
    ) -> Submit {
        let text = message(text);
        if self.selected.is_empty() {
            return Submit::Nothing("Pick an agent to send to.");
        }
        if text.is_empty() && !self.press_return {
            return Submit::Nothing("Type something to send.");
        }
        let busy: Vec<u64> = self
            .selected
            .iter()
            .copied()
            .filter(|id| working.contains(id))
            .collect();
        let new = busy
            .iter()
            .any(|id| !self.warned.as_ref().is_some_and(|w| w.contains(id)));
        if new {
            let names: Vec<String> = busy.iter().map(|&id| name(id)).collect();
            self.warned = Some(busy);
            return Submit::Warn(working_warning(&names));
        }
        Submit::Send(SendPlan {
            text: text.to_owned(),
            targets: self.selected.clone(),
            press_return: self.press_return,
        })
    }
}

/// The warning for working targets `names` (at least one): the text
/// lands in the middle of what they are doing.
pub fn working_warning(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let listed = if names.len() > SHOWN {
        format!(
            "{}, +{} more",
            names[..SHOWN].join(", "),
            names.len() - SHOWN
        )
    } else {
        names.join(", ")
    };
    match names.len() {
        1 => format!("{listed} is working: the text lands mid-turn. Send anyway?"),
        n => format!("{n} agents are working ({listed}): the text lands mid-turn. Send anyway?"),
    }
}

/// The Send button's title for `n` targets, and after a warning.
pub fn send_title(n: usize, warned: bool) -> String {
    if warned {
        return "Send Anyway".to_owned();
    }
    match n {
        0 | 1 => "Send".to_owned(),
        n => format!("Send to {n}"),
    }
}

/// What a click on a sidebar row does, by the keys held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClick {
    /// A plain click: the selection is cleared; show the agent.
    Show,
    /// ⌘ or ⇧ changed the selection; nothing else happens.
    Selected,
}

/// The sidebar rows picked with ⌘-click (one at a time) and ⇧-click (a
/// run from the last row clicked), as Finder's list does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowSelection {
    ids: Vec<u64>,
    /// Where a ⇧-click's run starts: the last row clicked.
    anchor: Option<u64>,
}

impl RowSelection {
    /// The rows picked, in the sidebar's order as last clicked.
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    pub fn contains(&self, id: u64) -> bool {
        self.ids.contains(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// A click on row `id` with ⌘ (`cmd`) or ⇧ (`shift`) held; `order`
    /// is the sidebar's rows as drawn, `current` the row shown focused
    /// (a ⌘-click adds to it, as to Finder's selected row).
    pub fn click(
        &mut self,
        id: u64,
        cmd: bool,
        shift: bool,
        order: &[u64],
        current: Option<u64>,
    ) -> RowClick {
        if shift {
            let from = self.anchor.or(current).filter(|a| order.contains(a));
            let run: Vec<u64> = match (from, order.iter().position(|&o| o == id)) {
                (Some(a), Some(to)) => {
                    let from = order.iter().position(|&o| o == a).unwrap_or(to);
                    let (lo, hi) = (from.min(to), from.max(to));
                    order[lo..=hi].to_vec()
                }
                _ => vec![id],
            };
            for r in run {
                if !self.ids.contains(&r) {
                    self.ids.push(r);
                }
            }
            if self.anchor.is_none() {
                self.anchor = from;
            }
            return RowClick::Selected;
        }
        if cmd {
            if self.ids.is_empty()
                && let Some(c) = current.filter(|&c| c != id)
            {
                self.ids.push(c);
            }
            if let Some(i) = self.ids.iter().position(|&s| s == id) {
                self.ids.remove(i);
            } else {
                self.ids.push(id);
            }
            self.anchor = Some(id);
            return RowClick::Selected;
        }
        self.ids.clear();
        self.anchor = Some(id);
        RowClick::Show
    }

    /// Clears the selection (Esc, or a plain click).
    pub fn clear(&mut self) {
        self.ids.clear();
    }

    /// Drops rows that are gone (`keep` says which stay).
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.ids.retain(|&id| keep(id));
        if self.anchor.is_some_and(|a| !keep(a)) {
            self.anchor = None;
        }
    }

    /// Who Send to… from row `id`'s menu targets: the selection if `id`
    /// is in it (and it has more than one row), else `id` alone.
    pub fn targets_from(&self, id: u64) -> Vec<u64> {
        if self.ids.len() > 1 && self.contains(id) {
            self.ids.clone()
        } else {
            vec![id]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::picker::{Picker, Recents};
    use fleet_proto::AgentInfo;

    fn row(id: u64, state: AgentState, created: i64) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "/usr/bin/claude".into(),
            cwd: format!("/Users/me/p{id}"),
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

    fn home() -> &'static Path {
        Path::new("/Users/me")
    }

    #[test]
    fn running_agents_only_newest_first_found_by_label() {
        let mut reviewer = row(1, AgentState::Running, 1);
        reviewer
            .info
            .labels
            .insert("role".into(), "reviewer".into());
        reviewer.info.labels.insert("name".into(), "rev".into());
        let agents = [
            reviewer,
            row(2, AgentState::Exited, 2),
            row(3, AgentState::Running, 3),
            row(4, AgentState::Lost, 4),
        ];
        let statuses = HashMap::from([(1, AgentStatus::Working)]);
        let items = send_items(&agents, &statuses, home());
        let keys: Vec<_> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["agent:3", "agent:1"]);
        assert_eq!(items[1].title, "rev");
        assert_eq!(items[1].detail, "#1 · ~/p1 · Working");
        assert_eq!(items[0].detail, "#3 · ~/p3");
        let mut p = Picker::new(items, Recents::default());
        p.set_query("role=reviewer");
        assert_eq!(p.ranked().len(), 1);
        assert_eq!(p.selected_item().unwrap().key, "agent:1");
        assert_eq!(key_agent("agent:12"), Some(12));
        assert_eq!(key_agent("folder:/x"), None);
        assert_eq!(agent_key(12), "agent:12");
    }

    #[test]
    fn text_goes_as_typed_and_multiline_as_a_bracketed_paste() {
        assert_eq!(encode_text("run the tests", true), b"run the tests");
        assert_eq!(encode_text("a\nb", false), b"a\nb", "as fleet send");
        assert_eq!(encode_text("a\nb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(message("hi\n\n"), "hi");
        assert_eq!(message("  a\nb\r\n"), "  a\nb");
    }

    fn names(id: u64) -> String {
        format!("agent-{id}")
    }

    #[test]
    fn send_needs_a_target_and_something_to_send() {
        let idle = HashSet::new();
        let mut form = SendForm::new([]);
        assert!(form.press_return, "Return after is on by default");
        assert!(matches!(
            form.submit("hi", &idle, names),
            Submit::Nothing(_)
        ));
        form.toggle(3);
        form.toggle_return();
        assert!(matches!(
            form.submit("\n", &idle, names),
            Submit::Nothing(_)
        ));
        // Just a Return is something to send.
        form.toggle_return();
        let Submit::Send(plan) = form.submit("", &idle, names) else {
            panic!("a lone Return is sent");
        };
        assert!(plan.press_return);
        assert!(!plan.return_delayed());
        let Submit::Send(plan) = form.submit("rebase on main\n", &idle, names) else {
            panic!("sent");
        };
        assert_eq!(
            plan,
            SendPlan {
                text: "rebase on main".into(),
                targets: vec![3],
                press_return: true
            }
        );
        assert!(plan.return_delayed());
    }

    #[test]
    fn working_targets_are_warned_about_once() {
        let working = HashSet::from([2, 5]);
        let mut form = SendForm::new([1, 2, 2]);
        assert_eq!(form.selected(), [1, 2]);
        let Submit::Warn(w) = form.submit("stop", &working, names) else {
            panic!("warned");
        };
        assert_eq!(
            w,
            "agent-2 is working: the text lands mid-turn. Send anyway?"
        );
        assert!(form.warning_shown());
        assert_eq!(send_title(2, form.warning_shown()), "Send Anyway");
        // Send again: sent anyway.
        assert!(matches!(
            form.submit("stop", &working, names),
            Submit::Send(_)
        ));
        // Another working target: warned again, naming both.
        form.toggle(5);
        assert!(!form.warning_shown(), "a change withdraws the warning");
        let Submit::Warn(w) = form.submit("stop", &working, names) else {
            panic!("warned");
        };
        assert!(w.starts_with("2 agents are working (agent-2, agent-5)"));
        // Nobody working: no warning.
        let mut form = SendForm::new([1]);
        assert!(matches!(form.submit("x", &working, names), Submit::Send(_)));
    }

    #[test]
    fn long_warnings_are_cut_short() {
        let n: Vec<String> = (1..=5).map(names).collect();
        assert_eq!(
            working_warning(&n),
            "5 agents are working (agent-1, agent-2, agent-3, +2 more): \
             the text lands mid-turn. Send anyway?"
        );
        assert_eq!(send_title(0, false), "Send");
        assert_eq!(send_title(1, false), "Send");
        assert_eq!(send_title(4, false), "Send to 4");
    }

    #[test]
    fn ended_targets_drop_out() {
        let mut form = SendForm::new([1, 2, 3]);
        form.submit("x", &HashSet::from([2]), names);
        form.retain(|id| id != 3);
        assert_eq!(form.selected(), [1, 2]);
        assert!(!form.warning_shown());
    }

    #[test]
    fn rows_are_picked_with_cmd_and_shift_like_finder() {
        let order = [10, 11, 12, 13, 14];
        let mut sel = RowSelection::default();
        // A plain click shows the agent and starts a run.
        assert_eq!(sel.click(11, false, false, &order, None), RowClick::Show);
        assert!(sel.is_empty());
        // ⇧-click: the run from the last row clicked.
        assert_eq!(
            sel.click(13, false, true, &order, Some(11)),
            RowClick::Selected
        );
        assert_eq!(sel.ids(), [11, 12, 13]);
        // ⌘-click toggles one.
        sel.click(12, true, false, &order, Some(11));
        assert_eq!(sel.ids(), [11, 13]);
        sel.click(14, true, false, &order, Some(11));
        assert_eq!(sel.ids(), [11, 13, 14]);
        assert_eq!(sel.targets_from(13), [11, 13, 14]);
        assert_eq!(sel.targets_from(10), [10], "a row outside: itself");
        // A plain click clears it.
        assert_eq!(
            sel.click(10, false, false, &order, Some(11)),
            RowClick::Show
        );
        assert!(sel.is_empty());
        // A first ⌘-click adds to the focused row.
        sel.click(12, true, false, &order, Some(10));
        assert_eq!(sel.ids(), [10, 12]);
        // ⇧-click upwards, from the last ⌘-clicked row.
        let mut up = RowSelection::default();
        up.click(13, true, false, &order, None);
        up.click(11, false, true, &order, None);
        assert_eq!(up.ids(), [13, 11, 12]);
        // Gone rows drop out.
        up.retain(|id| id != 12);
        assert_eq!(up.ids(), [13, 11]);
        up.clear();
        assert!(up.is_empty());
        // One row selected: Send to… targets the row clicked.
        let mut one = RowSelection::default();
        one.click(12, true, false, &order, None);
        assert_eq!(one.targets_from(12), [12]);
    }
}
