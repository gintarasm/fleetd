//! The shortcut cheat sheet (⌘/), the recently closed panes ⌘⇧T reopens,
//! and what an empty pane shows (issue #46).
//!
//! The sheet is built from the key bindings the app registered, never
//! from a list of its own, so a binding added anywhere shows up in it.

/// A key binding as registered: gpui's keystrokes (`"cmd-shift-d"`, a
/// chord space-separated), its action's name (`"fleet::SplitDown"`) and
/// key context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding<'a> {
    pub keys: &'a str,
    pub action: &'a str,
    pub context: Option<&'a str>,
}

/// One line of the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The keys as glyphs (`"⇧⌘D"`).
    pub keys: String,
    pub title: String,
}

/// The bindings of one key context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
}

/// The sections in order: the window's bindings (no context, or the
/// workspace's), the terminal's, the text fields', then any other context
/// in the order first bound. A binding of keys already bound in the same
/// section replaces it, as the later binding wins in gpui.
pub fn cheat_sheet<'a>(bindings: impl IntoIterator<Item = Binding<'a>>) -> Vec<Section> {
    let mut sections: Vec<Section> = ["Window", "Terminal", "Text Fields"]
        .into_iter()
        .map(|title| Section {
            title: title.to_owned(),
            rows: Vec::new(),
        })
        .collect();
    for b in bindings {
        let title = match b.context {
            None | Some("Workspace") => "Window".to_owned(),
            // A terminal binding narrowed further ("Terminal && finding").
            Some(c) if c == "Terminal" || c.starts_with("Terminal &&") => "Terminal".to_owned(),
            Some("TextField") => "Text Fields".to_owned(),
            Some(other) => humanize(other),
        };
        let index = match sections.iter().position(|s| s.title == title) {
            Some(i) => i,
            None => {
                sections.push(Section {
                    title,
                    rows: Vec::new(),
                });
                sections.len() - 1
            }
        };
        let rows = &mut sections[index].rows;
        let row = Row {
            keys: key_glyphs(b.keys),
            title: action_title(b.action),
        };
        match rows.iter().position(|r| r.keys == row.keys) {
            Some(i) => rows[i] = row,
            None => rows.push(row),
        }
    }
    sections.retain(|s| !s.rows.is_empty());
    sections
}

/// gpui keystrokes as macOS draws them: modifiers in menu order
/// (⌃⌥⇧⌘) before the key; a chord's keystrokes space-separated.
pub fn key_glyphs(keys: &str) -> String {
    keys.split(' ')
        .filter(|s| !s.is_empty())
        .map(keystroke_glyphs)
        .collect::<Vec<_>>()
        .join(" ")
}

fn keystroke_glyphs(stroke: &str) -> String {
    // The key is what follows the last `-`, unless the key is `-` itself.
    let (mods, key) = match stroke.strip_suffix("--") {
        Some(mods) => (mods, "-"),
        None => match stroke.rsplit_once('-') {
            Some((mods, key)) => (mods, key),
            None => ("", stroke),
        },
    };
    let has = |m: &str| mods.split('-').any(|p| p == m);
    let mut out = String::new();
    for (name, glyph) in [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")] {
        if has(name) {
            out.push_str(glyph);
        }
    }
    if has("fn") {
        out.insert_str(0, "fn ");
    }
    out.push_str(&key_glyph(key));
    out
}

fn key_glyph(key: &str) -> String {
    let named = match key {
        "enter" => "↩",
        "tab" => "⇥",
        "escape" => "⎋",
        "backspace" => "⌫",
        "delete" => "⌦",
        "space" => "Space",
        "left" => "←",
        "right" => "→",
        "up" => "↑",
        "down" => "↓",
        "home" => "↖",
        "end" => "↘",
        "pageup" => "⇞",
        "pagedown" => "⇟",
        _ => return key.to_uppercase(),
    };
    named.to_owned()
}

/// An action's title: its name without the namespace, in words
/// (`"fleet::FocusPane1"` is "Focus Pane 1").
pub fn action_title(action: &str) -> String {
    let name = action.rsplit("::").next().unwrap_or(action);
    humanize(name)
}

/// `"ReopenClosedPane"` → "Reopen Closed Pane", `"FocusPane1"` →
/// "Focus Pane 1"; underscores become spaces.
fn humanize(name: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if c == '_' {
            out.push(' ');
            prev = Some(' ');
            continue;
        }
        let boundary = match prev {
            Some(p) if p != ' ' => {
                (c.is_uppercase() && !p.is_uppercase())
                    || (c.is_ascii_digit() && !p.is_ascii_digit())
            }
            _ => false,
        };
        if boundary {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// The agents of the panes closed lately, newest last; ⌘⇧T takes them
/// back newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClosedPanes {
    agents: Vec<u64>,
}

impl ClosedPanes {
    /// How many are kept.
    pub const CAP: usize = 10;

    /// A pane showing `agent_id` closed. An agent already in the stack
    /// moves to its top.
    pub fn push(&mut self, agent_id: u64) {
        self.agents.retain(|&a| a != agent_id);
        self.agents.push(agent_id);
        if self.agents.len() > Self::CAP {
            self.agents.remove(0);
        }
    }

    /// The newest agent that is still `known`; the ones above it, no
    /// longer known, are dropped.
    pub fn pop(&mut self, known: impl Fn(u64) -> bool) -> Option<u64> {
        while let Some(id) = self.agents.pop() {
            if known(id) {
                return Some(id);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Forgets them all (another fleetd's agents are other agents).
    pub fn clear(&mut self) {
        self.agents.clear();
    }
}

/// What a pane with no agent shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyPane {
    /// The pane shows an agent.
    None,
    /// A spawn into it is on its way.
    Starting,
    /// The one-line hint.
    Hint,
    /// The first run: the fleetd listed no agents at all.
    Welcome,
}

/// `no_agents`: the agent list arrived and is empty.
pub fn empty_pane(has_agent: bool, starting: bool, no_agents: bool) -> EmptyPane {
    match (has_agent, starting, no_agents) {
        (true, _, _) => EmptyPane::None,
        (false, true, _) => EmptyPane::Starting,
        (false, false, true) => EmptyPane::Welcome,
        (false, false, false) => EmptyPane::Hint,
    }
}

/// The text of a [`EmptyPane::Hint`].
pub const HINT: &str = "Select an agent or press ⌘N";
