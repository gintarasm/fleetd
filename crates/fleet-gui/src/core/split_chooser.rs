//! The split chooser (issue #30): a pane opened by a split asks what to
//! start in it. 1 a new agent (the default command), 2 an agent not on
//! screen (from a fuzzy picker), 3 a plain terminal. Esc closes the pane;
//! in the agent list it goes back to the choices.

use std::collections::HashSet;
use std::path::Path;

use super::palette::agent_item;
use super::picker::PickerItem;
use super::session::AgentRow;
use super::sidebar::{program_name, tilde};

/// What the new pane can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Choice {
    NewAgent,
    Agent,
    Terminal,
}

impl Choice {
    /// The choices, in their keys' order (1, 2, 3).
    pub const ALL: [Choice; 3] = [Choice::NewAgent, Choice::Agent, Choice::Terminal];

    /// Its key, 1-based.
    pub fn number(self) -> usize {
        match self {
            Choice::NewAgent => 1,
            Choice::Agent => 2,
            Choice::Terminal => 3,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Choice::NewAgent => "New agent",
            Choice::Agent => "Existing agent…",
            Choice::Terminal => "Terminal",
        }
    }
}

/// A choice as the pane lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceRow {
    pub choice: Choice,
    pub title: &'static str,
    /// What it starts, and where (`claude in ~/src`).
    pub detail: String,
    /// It can be picked (Existing agent needs an agent off screen).
    pub enabled: bool,
}

/// The three rows. `command` is `[gui] default_command` (empty: the New
/// agent sheet asks); `dir` the folder both start in (`""`: the daemon's
/// own); `off_screen` how many agents the list would offer.
pub fn choice_rows(
    command: &[String],
    dir: &str,
    home: &Path,
    off_screen: usize,
) -> Vec<ChoiceRow> {
    let place = if dir.is_empty() {
        String::new()
    } else {
        format!(" in {}", tilde(dir, home))
    };
    Choice::ALL
        .into_iter()
        .map(|choice| {
            let (detail, enabled) = match choice {
                Choice::NewAgent => match command.first() {
                    Some(program) => (format!("{}{place}", program_name(program)), true),
                    None => ("choose a command and folder".to_owned(), true),
                },
                Choice::Agent => match off_screen {
                    0 => ("every agent is on screen".to_owned(), false),
                    1 => ("1 not on screen".to_owned(), true),
                    n => (format!("{n} not on screen"), true),
                },
                Choice::Terminal => (format!("shell{place}"), true),
            };
            ChoiceRow {
                choice,
                title: choice.title(),
                detail,
                enabled,
            }
        })
        .collect()
}

/// The agents the list offers: every agent no pane shows, newest first,
/// as the palette lists them (name, folder, status).
pub fn off_screen_agents(
    agents: &[AgentRow],
    on_screen: &HashSet<u64>,
    home: &Path,
) -> Vec<(u64, PickerItem)> {
    let mut rows: Vec<&AgentRow> = agents
        .iter()
        .filter(|r| !on_screen.contains(&r.info.id))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    rows.into_iter()
        .map(|r| (r.info.id, agent_item(r, home)))
        .collect()
}

/// Which part of the chooser shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Choices,
    /// The agent list (a picker).
    Agents,
}

/// What a key or click asks the shell to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Start a new agent or a terminal in the pane.
    Start(Choice),
    /// The agent list opened.
    ShowAgents,
    /// The agent list closed, back to the choices.
    Back,
    /// Close the pane: the split is cancelled.
    Cancel,
    /// The key was the chooser's, with nothing more to do (the selection
    /// moved, or a disabled choice).
    Stay,
}

/// The chooser's state: the stage and the selected choice.
#[derive(Clone, Debug)]
pub struct SplitChooser {
    rows: Vec<ChoiceRow>,
    stage: Stage,
    selected: usize,
}

impl SplitChooser {
    pub fn new(rows: Vec<ChoiceRow>) -> Self {
        SplitChooser {
            rows,
            stage: Stage::Choices,
            selected: 0,
        }
    }

    pub fn rows(&self) -> &[ChoiceRow] {
        &self.rows
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The selected row (an index into [`SplitChooser::rows`]).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Selects row `row` (a hover); a disabled row is not selected.
    pub fn select(&mut self, row: usize) {
        if self.rows.get(row).is_some_and(|r| r.enabled) {
            self.selected = row;
        }
    }

    /// Picks `choice` (its key or a click); `None` if it is disabled.
    pub fn pick(&mut self, choice: Choice) -> Option<Outcome> {
        let row = self.rows.iter().position(|r| r.choice == choice)?;
        if !self.rows[row].enabled {
            return None;
        }
        self.selected = row;
        Some(match choice {
            Choice::Agent => {
                self.stage = Stage::Agents;
                Outcome::ShowAgents
            }
            c => Outcome::Start(c),
        })
    }

    /// The agent list was dismissed: back to the choices.
    pub fn back(&mut self) -> Outcome {
        self.stage = Stage::Choices;
        Outcome::Back
    }

    /// A key in the choices (a key name as the keymap writes it, without
    /// modifiers): 1/2/3 pick, ↑/↓ select (wrapping, skipping a disabled
    /// row), Enter picks the selected, Esc cancels. `None`: not the chooser's key.
    pub fn key(&mut self, key: &str) -> Option<Outcome> {
        if self.stage != Stage::Choices {
            return None;
        }
        match key {
            "1" | "2" | "3" => {
                let n: usize = key.parse().ok()?;
                // A disabled choice still takes its key.
                Some(self.pick(Choice::ALL[n - 1]).unwrap_or(Outcome::Stay))
            }
            "up" => {
                self.step(-1);
                Some(Outcome::Stay)
            }
            "down" => {
                self.step(1);
                Some(Outcome::Stay)
            }
            "enter" => {
                let choice = self.rows.get(self.selected)?.choice;
                Some(self.pick(choice).unwrap_or(Outcome::Stay))
            }
            "escape" => Some(Outcome::Cancel),
            _ => None,
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.rows.len() as isize;
        let mut at = self.selected as isize;
        for _ in 0..n {
            at = (at + delta).rem_euclid(n);
            if self.rows[at as usize].enabled {
                self.selected = at as usize;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::{AgentInfo, AgentState};

    fn home() -> &'static Path {
        Path::new("/Users/me")
    }

    fn row(id: u64, program: &str, cwd: &str, created: i64) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: program.into(),
            cwd: cwd.into(),
            created_unix_ms: created,
            ..Default::default()
        };
        info.set_state(AgentState::Running);
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    fn chooser(off_screen: usize) -> SplitChooser {
        SplitChooser::new(choice_rows(
            &["claude".into()],
            "/Users/me/src",
            home(),
            off_screen,
        ))
    }

    #[test]
    fn rows_say_what_starts_where() {
        let rows = choice_rows(
            &["/usr/bin/claude".into(), "--yolo".into()],
            "/Users/me/src",
            home(),
            2,
        );
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.choice.number(), r.title, r.detail.as_str(), r.enabled))
            .collect();
        assert_eq!(
            got,
            [
                (1, "New agent", "claude in ~/src", true),
                (2, "Existing agent…", "2 not on screen", true),
                (3, "Terminal", "shell in ~/src", true),
            ]
        );
    }

    #[test]
    fn rows_without_a_command_a_folder_or_agents_off_screen() {
        let rows = choice_rows(&[], "", home(), 0);
        assert_eq!(rows[0].detail, "choose a command and folder");
        assert!(rows[0].enabled);
        assert_eq!(rows[1].detail, "every agent is on screen");
        assert!(!rows[1].enabled);
        // A remote daemon's own folder: nothing to name.
        assert_eq!(rows[2].detail, "shell");
        assert_eq!(choice_rows(&[], "", home(), 1)[1].detail, "1 not on screen");
    }

    #[test]
    fn keys_one_two_three_pick() {
        let mut c = chooser(1);
        assert_eq!(c.key("1"), Some(Outcome::Start(Choice::NewAgent)));
        let mut c = chooser(1);
        assert_eq!(c.key("3"), Some(Outcome::Start(Choice::Terminal)));
        let mut c = chooser(1);
        assert_eq!(c.key("2"), Some(Outcome::ShowAgents));
        assert_eq!(c.stage(), Stage::Agents);
        // The list owns the keys now.
        assert_eq!(c.key("1"), None);
        assert_eq!(c.key("escape"), None);
        assert_eq!(c.back(), Outcome::Back);
        assert_eq!(c.stage(), Stage::Choices);
        assert_eq!(c.selected(), 1);
    }

    #[test]
    fn esc_cancels_and_other_keys_are_not_the_choosers() {
        let mut c = chooser(1);
        assert_eq!(c.key("escape"), Some(Outcome::Cancel));
        assert_eq!(c.key("a"), None);
        assert_eq!(c.key("4"), None);
    }

    #[test]
    fn a_disabled_choice_takes_its_key_but_does_nothing() {
        let mut c = chooser(0);
        assert_eq!(c.key("2"), Some(Outcome::Stay));
        assert_eq!(c.stage(), Stage::Choices);
        assert_eq!(c.pick(Choice::Agent), None);
    }

    #[test]
    fn arrows_select_skipping_disabled_and_enter_picks() {
        let mut c = chooser(0);
        assert_eq!(c.selected(), 0);
        assert_eq!(c.key("down"), Some(Outcome::Stay));
        assert_eq!(c.selected(), 2);
        c.key("down");
        assert_eq!(c.selected(), 0);
        c.key("up");
        assert_eq!(c.selected(), 2);
        assert_eq!(c.key("enter"), Some(Outcome::Start(Choice::Terminal)));

        let mut c = chooser(3);
        c.key("down");
        assert_eq!(c.key("enter"), Some(Outcome::ShowAgents));
        // Hover selects, but not a disabled row.
        let mut c = chooser(0);
        c.select(2);
        assert_eq!(c.selected(), 2);
        c.select(1);
        c.select(9);
        assert_eq!(c.selected(), 2);
    }

    #[test]
    fn the_list_leaves_out_agents_on_screen_newest_first() {
        let agents = [
            row(1, "claude", "/Users/me/a", 1),
            row(2, "codex", "/Users/me/b", 3),
            row(3, "zsh", "/Users/me/c", 2),
        ];
        let got = off_screen_agents(&agents, &HashSet::from([1]), home());
        let ids: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [2, 3]);
        assert_eq!(got[0].1.title, "codex in ~/b");
        assert_eq!(got[0].1.detail, "~/b · running");
        assert_eq!(got[0].1.key, "agent:2");
        assert!(off_screen_agents(&agents, &HashSet::from([1, 2, 3]), home()).is_empty());
    }
}
