//! Command presets, the first prompt and Duplicate Agent (ADR 0084):
//! what the New agent sheet is prefilled with, and the presets the sheet,
//! the split chooser and the palette offer.
//!
//! A preset is a name, a command and labels, from `[[gui.presets]]` in
//! `config.toml`; without that key, the agents found on `PATH`
//! ([`detect_presets`], run once on first use, never per frame).

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use fleet_client::names::{NAME_LABEL, default_name};
use fleet_proto::AgentInfo;
use serde::Deserialize;

use super::labels::read_only;
use super::spawn::SpawnRequest;
use super::target::Target;

/// How many presets the split chooser lists (keys 4 to 9).
pub const CHOOSER_PRESETS: usize = 6;

/// A named command with labels: `[[gui.presets]]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Preset {
    pub name: String,
    /// Program and arguments, as `default_command`.
    pub command: Vec<String>,
    /// Labels set on the agent; never `name` (the spawn generates one).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Preset {
    fn new(name: &str, command: &[&str]) -> Preset {
        Preset {
            name: name.to_owned(),
            command: command.iter().map(|w| (*w).to_owned()).collect(),
            labels: BTreeMap::new(),
        }
    }

    /// The command as the sheet's Command field shows it (shell quoting).
    pub fn command_line(&self) -> String {
        shell_words::join(&self.command)
    }

    /// The labels as the sheet's Labels field shows them.
    pub fn labels_text(&self) -> String {
        labels_text(self.labels.iter())
    }
}

/// The default presets' candidates: a name and a command whose program
/// must be found for it to be offered.
const CANDIDATES: [(&str, &[&str]); 4] = [
    ("Claude", &["claude"]),
    ("Claude (continue)", &["claude", "--continue"]),
    ("Codex", &["codex"]),
    ("omp", &["omp"]),
];

/// The default presets: each candidate whose program `found` says is
/// there, in order.
pub fn default_presets(found: impl Fn(&str) -> bool) -> Vec<Preset> {
    CANDIDATES
        .iter()
        .filter(|(_, command)| found(command[0]))
        .map(|(name, command)| Preset::new(name, command))
        .collect()
}

/// Where a program is looked for: `PATH`, then the folders agents are
/// usually installed in, which an app started from the Finder (with the
/// system's short `PATH`) would miss.
pub fn search_dirs(path_var: &OsStr, home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path_var).collect();
    for extra in [
        home.join(".local/bin"),
        home.join(".claude/local"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
        home.join(".npm-global/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ] {
        if !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    dirs
}

/// The presets when the config sets none: locally the candidates found
/// in [`search_dirs`] (a few `stat`s; call it once, off the frame path);
/// on an ssh target, whose `PATH` the GUI cannot see, every candidate.
pub fn detect_presets(target: &Target, path_var: &OsStr, home: &Path) -> Vec<Preset> {
    match target {
        Target::Local => {
            let dirs = search_dirs(path_var, home);
            default_presets(|program| dirs.iter().any(|d| d.join(program).is_file()))
        }
        Target::Ssh { .. } => default_presets(|_| true),
    }
}

/// The Settings window's note under its preset list.
pub fn presets_note(detected: bool) -> &'static str {
    if detected {
        "The agents found on PATH; list your own as [[gui.presets]] in config.toml"
    } else {
        "From [[gui.presets]] in config.toml (name, command, labels)"
    }
}

/// The presets that can be offered: a name and a command, the first of
/// each name, and no read-only label (`name`, `fleet.*`; the sheet's
/// Labels field refuses them too).
pub fn usable(presets: &[Preset]) -> Vec<Preset> {
    let mut out: Vec<Preset> = Vec::new();
    for p in presets {
        let name = p.name.trim();
        if name.is_empty()
            || p.command.first().is_none_or(|w| w.trim().is_empty())
            || out.iter().any(|o| o.name == name)
        {
            continue;
        }
        let mut p = p.clone();
        p.name = name.to_owned();
        p.labels.retain(|k, _| !read_only(k));
        out.push(p);
    }
    out
}

/// Labels as `k=v, k=v`, sorted, leaving out the name (it has its own
/// field) and the core's `fleet.*` keys.
pub fn labels_text<'a>(labels: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    let mut pairs: Vec<String> = labels
        .filter(|(k, _)| !read_only(k))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    pairs.sort();
    pairs.join(", ")
}

/// The first prompt, appended as the command's last argument (as in
/// `claude "fix the build"`); blank adds nothing.
pub fn append_prompt(args: &mut Vec<String>, prompt: &str) {
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        args.push(prompt.to_owned());
    }
}

/// The command line a generated name is drawn from: the command with
/// the prompt quoted after it, so the prompt's words name the agent.
pub fn name_source(command: &str, prompt: &str) -> String {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        command.to_owned()
    } else {
        format!("{command} {}", shell_words::quote(prompt))
    }
}

/// What the New agent sheet opens with, besides its defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefill {
    /// The Command field; `None`: the default command.
    pub command: Option<String>,
    /// The Directory field; `None`: the default folder.
    pub dir: Option<String>,
    /// The Labels field.
    pub labels: String,
}

impl Prefill {
    /// A preset's command and labels; the folder stays the default.
    pub fn preset(p: &Preset) -> Prefill {
        Prefill {
            command: Some(p.command_line()),
            dir: None,
            labels: p.labels_text(),
        }
    }

    /// Duplicate Agent: the agent's command, folder and labels. Not its
    /// name: the sheet generates one that no live agent has (ADR 0052),
    /// the agent itself included.
    pub fn duplicate(info: &AgentInfo) -> Prefill {
        let mut words = vec![info.program.clone()];
        words.extend(info.args.iter().cloned());
        Prefill {
            command: Some(shell_words::join(&words)),
            dir: (!info.cwd.is_empty()).then(|| info.cwd.clone()),
            labels: labels_text(info.labels.iter()),
        }
    }
}

/// A preset started straight away (the split chooser): its command and
/// labels in `cwd`, with a generated name unique among `taken`.
pub fn preset_request(p: &Preset, cwd: String, taken: &[&str]) -> Option<SpawnRequest> {
    let (program, args) = p.command.split_first()?;
    let mut labels: HashMap<String, String> = p
        .labels
        .iter()
        .filter(|(k, _)| !read_only(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.insert(
        NAME_LABEL.to_owned(),
        default_name(program, args, &cwd, taken),
    );
    Some(SpawnRequest {
        program: program.clone(),
        args: args.to_vec(),
        cwd,
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_candidates_found() {
        let names = |found: &dyn Fn(&str) -> bool| -> Vec<String> {
            default_presets(found).into_iter().map(|p| p.name).collect()
        };
        assert_eq!(
            names(&|p| p == "claude" || p == "omp"),
            ["Claude", "Claude (continue)", "omp"]
        );
        assert!(names(&|_| false).is_empty());
        let all = default_presets(|_| true);
        assert_eq!(all.len(), 4);
        assert_eq!(all[1].command, ["claude", "--continue"]);
    }

    #[test]
    fn detection_looks_in_path_then_the_usual_folders() {
        let home = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        std::fs::write(on_path.path().join("codex"), "").unwrap();
        std::fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        std::fs::write(home.path().join(".local/bin/claude"), "").unwrap();
        let path = std::env::join_paths([on_path.path()]).unwrap();
        let found: Vec<String> = detect_presets(&Target::Local, &path, home.path())
            .into_iter()
            .map(|p| p.name)
            .filter(|n| n != "omp")
            .collect();
        assert_eq!(found, ["Claude", "Claude (continue)", "Codex"]);
        // A remote PATH is not seen: every candidate.
        let remote = Target::Ssh { dest: "box".into() };
        assert_eq!(
            detect_presets(&remote, OsStr::new(""), home.path()).len(),
            4
        );
        // PATH comes first, the extras once.
        let dirs = search_dirs(&path, home.path());
        assert_eq!(dirs[0], on_path.path());
        let path2 = std::env::join_paths([home.path().join(".local/bin")]).unwrap();
        let dirs2 = search_dirs(&path2, home.path());
        assert_eq!(
            dirs2
                .iter()
                .filter(|d| **d == home.path().join(".local/bin"))
                .count(),
            1
        );
    }

    #[test]
    fn config_presets_parse_and_unusable_ones_are_dropped() {
        #[derive(Deserialize)]
        struct F {
            presets: Vec<Preset>,
        }
        let f: F = toml::from_str(
            r#"
            [[presets]]
            name = "Review"
            command = ["claude", "--model", "opus"]
            labels = { role = "reviewer", name = "x", "fleet.depth" = "1" }
            [[presets]]
            name = " "
            command = ["sh"]
            [[presets]]
            name = "Empty"
            command = []
            [[presets]]
            name = "Review"
            command = ["other"]
            "#,
        )
        .unwrap();
        let got = usable(&f.presets);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command_line(), "claude --model opus");
        assert_eq!(got[0].labels_text(), "role=reviewer");
    }

    #[test]
    fn the_prompt_is_the_last_argument_and_names_the_agent() {
        let mut args = vec!["--continue".to_owned()];
        append_prompt(&mut args, "   ");
        assert_eq!(args, ["--continue"]);
        append_prompt(&mut args, "  fix the \"LTO\" build ");
        assert_eq!(args, ["--continue", "fix the \"LTO\" build"]);
        assert_eq!(name_source("claude", ""), "claude");
        let line = name_source("claude", "fix the SIGTERM race");
        assert_eq!(
            shell_words::split(&line).unwrap(),
            ["claude", "fix the SIGTERM race"]
        );
    }

    #[test]
    fn duplicate_keeps_command_folder_and_labels_but_not_the_name() {
        let mut info = AgentInfo {
            program: "claude".into(),
            args: vec!["--continue".into(), "fix the build".into()],
            cwd: "/w/fleetd".into(),
            ..Default::default()
        };
        info.labels.insert("name".into(), "build-otter".into());
        info.labels.insert("role".into(), "fixer".into());
        info.labels.insert("fleet.parent".into(), "3".into());
        info.labels.insert("kind".into(), "x".into());
        let p = Prefill::duplicate(&info);
        assert_eq!(
            p.command.as_deref(),
            Some("claude --continue 'fix the build'")
        );
        assert_eq!(p.dir.as_deref(), Some("/w/fleetd"));
        assert_eq!(p.labels, "kind=x, role=fixer");
        // The command round-trips through the sheet's parse.
        let words = shell_words::split(p.command.as_deref().unwrap()).unwrap();
        assert_eq!(words[2], "fix the build");
    }

    #[test]
    fn a_preset_prefills_command_and_labels() {
        let mut p = Preset::new("Review", &["claude", "--model", "opus 4"]);
        p.labels.insert("role".into(), "reviewer".into());
        let f = Prefill::preset(&p);
        assert_eq!(f.command.as_deref(), Some("claude --model 'opus 4'"));
        assert_eq!(f.dir, None);
        assert_eq!(f.labels, "role=reviewer");
    }

    #[test]
    fn a_preset_request_is_named_and_unique() {
        let mut p = Preset::new("omp", &["omp"]);
        p.labels.insert("role".into(), "worker".into());
        let a = preset_request(&p, "/w/fleetd".into(), &[]).unwrap();
        assert_eq!(a.program, "omp");
        assert_eq!(a.labels["role"], "worker");
        let name = a.labels[NAME_LABEL].clone();
        let b = preset_request(&p, "/w/fleetd".into(), &[name.as_str()]).unwrap();
        assert_ne!(b.labels[NAME_LABEL], name);
        assert!(preset_request(&Preset::new("x", &[]), "/".into(), &[]).is_none());
    }
}
