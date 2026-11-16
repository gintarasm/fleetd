//! The split chooser (issue #30): a pane opened by a split asks what to
//! start in it. 1 a new agent (the default command), 2 an agent not on
//! screen (from a fuzzy picker), 3 a plain terminal, then 4 to 9 the
//! command presets (ADR 0084). Esc closes the pane; in the agent list it
//! goes back to the choices.

use std::collections::HashSet;
use std::path::Path;

use super::palette::agent_item;
use super::picker::PickerItem;
use super::presets::{CHOOSER_PRESETS, Preset};
use super::session::AgentRow;
use super::sidebar::{program_name, tilde};

/// What the new pane can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Choice {
    NewAgent,
    Agent,
    Terminal,
    /// The preset at this index of the list the rows were made from.
    Preset(usize),
}

impl Choice {
    /// The fixed choices, in their keys' order (1, 2, 3); the presets
    /// follow.
    pub const ALL: [Choice; 3] = [Choice::NewAgent, Choice::Agent, Choice::Terminal];

    /// Its key, 1-based.
    pub fn number(self) -> usize {
        match self {
            Choice::NewAgent => 1,
            Choice::Agent => 2,
            Choice::Terminal => 3,
            Choice::Preset(i) => 4 + i,
        }
    }

    /// A fixed choice's title (a preset's is its name).
    pub fn title(self) -> &'static str {
        match self {
            Choice::NewAgent => "New agent",
            Choice::Agent => "Existing agent…",
            Choice::Terminal => "Terminal",
            Choice::Preset(_) => "",
        }
    }
}

/// A choice as the pane lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceRow {
    pub choice: Choice,
    pub title: String,
    /// What it starts, and where (`claude in ~/src`).
    pub detail: String,
    /// It can be picked (Existing agent needs an agent off screen).
    pub enabled: bool,
}

/// The three rows, then a row per preset (at most [`CHOOSER_PRESETS`]).
/// `command` is `[gui] default_command` (empty: the New agent sheet
/// asks); `dir` the folder they start in (`""`: the daemon's own);
/// `off_screen` how many agents the list would offer.
pub fn choice_rows(
    command: &[String],
    dir: &str,
    home: &Path,
    off_screen: usize,
    presets: &[Preset],
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
                Choice::Preset(_) => unreachable!("not in Choice::ALL"),
            };
            ChoiceRow {
                choice,
                title: choice.title().to_owned(),
                detail,
                enabled,
            }
        })
        .chain(
            presets
                .iter()
                .take(CHOOSER_PRESETS)
                .enumerate()
                .map(|(i, p)| ChoiceRow {
                    choice: Choice::Preset(i),
                    title: p.name.clone(),
                    detail: format!("{}{place}", p.command_line()),
                    enabled: true,
                }),
        )
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
    /// modifiers): 1/2/3 and a preset's 4 to 9 pick, ↑/↓ select (wrapping, skipping a disabled
    /// row), Enter picks the selected, Esc cancels. `None`: not the chooser's key.
    pub fn key(&mut self, key: &str) -> Option<Outcome> {
        if self.stage != Stage::Choices {
            return None;
        }
        match key {
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                let n: usize = key.parse().ok()?;
                let choice = self.rows.iter().find(|r| r.choice.number() == n)?.choice;
                // A disabled choice still takes its key.
                Some(self.pick(choice).unwrap_or(Outcome::Stay))
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
            &[],
        ))
    }

    #[test]
    fn rows_say_what_starts_where() {
        let rows = choice_rows(
            &["/usr/bin/claude".into(), "--yolo".into()],
            "/Users/me/src",
            home(),
            2,
            &[],
        );
        let got: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.choice.number(),
                    r.title.as_str(),
                    r.detail.as_str(),
                    r.enabled,
                )
            })
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
        let rows = choice_rows(&[], "", home(), 0, &[]);
        assert_eq!(rows[0].detail, "choose a command and folder");
        assert!(rows[0].enabled);
        assert_eq!(rows[1].detail, "every agent is on screen");
        assert!(!rows[1].enabled);
        // A remote daemon's own folder: nothing to name.
        assert_eq!(rows[2].detail, "shell");
        assert_eq!(
            choice_rows(&[], "", home(), 1, &[])[1].detail,
            "1 not on screen"
        );
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

    fn preset(name: &str, command: &[&str]) -> Preset {
        Preset {
            name: name.into(),
            command: command.iter().map(|w| (*w).to_owned()).collect(),
            labels: Default::default(),
        }
    }

    #[test]
    fn presets_follow_the_fixed_choices_with_keys_four_to_nine() {
        let presets: Vec<Preset> = (0..8)
            .map(|i| preset(&format!("P{i}"), &["omp", "--x"]))
            .collect();
        let rows = choice_rows(&["claude".into()], "/Users/me/src", home(), 0, &presets);
        // Three fixed rows, then six presets.
        assert_eq!(rows.len(), 3 + CHOOSER_PRESETS);
        assert_eq!(rows[3].choice, Choice::Preset(0));
        assert_eq!(rows[3].title, "P0");
        assert_eq!(rows[3].detail, "omp --x in ~/src");
        assert_eq!(rows.last().unwrap().choice.number(), 9);
        let mut c = SplitChooser::new(rows);
        assert_eq!(c.key("5"), Some(Outcome::Start(Choice::Preset(1))));
        // Arrows reach them; Enter starts one.
        let mut c2 = c.clone();
        c2.select(0);
        c2.key("up");
        assert_eq!(c2.key("enter"), Some(Outcome::Start(Choice::Preset(5))));
        // Without presets, 4 is not the chooser's.
        let mut plain = chooser(1);
        assert_eq!(plain.key("4"), None);
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
