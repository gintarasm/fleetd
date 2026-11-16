//! Agent names (ADR 0052): the `name` label, its generated default and
//! lookups by name. Clients share these so the CLI and the GUI agree.

use fleet_proto::{AgentInfo, AgentState};

/// The label key that holds an agent's name.
pub const NAME_LABEL: &str = "name";
/// A generated name is at most this many bytes.
pub const MAX_DEFAULT_LEN: usize = 32;
/// A given name is at most this many characters.
pub const MAX_NAME_CHARS: usize = 64;

/// The animals of generated names: short, distinct, lowercase.
pub const ANIMALS: &[&str] = &[
    "otter", "falcon", "badger", "heron", "fox", "lynx", "owl", "wren", "hare", "mole", "newt",
    "orca", "puffin", "raven", "seal", "stoat", "swift", "tapir", "toad", "vole", "wolf", "yak",
    "crane", "bison", "koala", "lemur", "marten", "moose", "ibex", "gecko", "finch", "egret",
    "dingo", "beaver", "bobcat", "panda", "quail", "robin", "shrew", "walrus",
];

/// Words that say little about a task: English stopwords, prompt
/// boilerplate and the most common task verbs.
const STOPWORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "agent",
    "agents",
    "all",
    "also",
    "and",
    "any",
    "are",
    "base",
    "been",
    "before",
    "being",
    "both",
    "branch",
    "but",
    "can",
    "change",
    "check",
    "code",
    "commit",
    "could",
    "create",
    "did",
    "does",
    "doing",
    "done",
    "each",
    "every",
    "file",
    "files",
    "first",
    "fix",
    "fleet",
    "for",
    "from",
    "get",
    "github",
    "had",
    "has",
    "have",
    "help",
    "her",
    "here",
    "his",
    "how",
    "implement",
    "into",
    "issue",
    "issues",
    "its",
    "just",
    "let",
    "like",
    "look",
    "make",
    "may",
    "merge",
    "might",
    "more",
    "most",
    "must",
    "need",
    "new",
    "not",
    "now",
    "off",
    "once",
    "one",
    "only",
    "open",
    "other",
    "our",
    "out",
    "over",
    "please",
    "pull",
    "push",
    "read",
    "repo",
    "repository",
    "request",
    "run",
    "same",
    "see",
    "she",
    "should",
    "some",
    "such",
    "sure",
    "task",
    "tasks",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "too",
    "two",
    "under",
    "update",
    "use",
    "used",
    "using",
    "very",
    "want",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "will",
    "with",
    "work",
    "working",
    "worktree",
    "would",
    "yes",
    "you",
    "your",
    "yours",
];

/// Programs whose plain run is a shell.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu",
];

/// A task word is cut to this many bytes.
const MAX_WORD_LEN: usize = 16;

/// The agent's name, if it has a non-empty one.
pub fn name_of(info: &AgentInfo) -> Option<&str> {
    info.labels
        .get(NAME_LABEL)
        .map(String::as_str)
        .filter(|n| !n.is_empty())
}

/// Checks a name given by the user or an agent and returns it trimmed:
/// 1 to [`MAX_NAME_CHARS`] characters, no control characters, not all
/// digits (that reads as an id).
pub fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a name must not be empty".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a name is at most {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("a name must not contain control characters".into());
    }
    if name.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{name:?} reads as an id: a name must not be all digits"
        ));
    }
    Ok(name.to_owned())
}

/// The file name of a path (`/bin/sh` → `sh`).
fn base_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// Lowercase ASCII letters and digits of `s`, cut to [`MAX_WORD_LEN`].
fn squash(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .take(MAX_WORD_LEN)
        .collect()
}

/// The most telling word of a spawn (ADR 0052), lowercase `[a-z0-9]`,
/// at most 16 bytes: the best prompt word (an acronym, else the longest,
/// else the earliest), else `i<n>` for an issue reference `#<n>`, else
/// `shell` for a plain shell, else the cwd's basename, else the program.
pub fn task_word(program: &str, args: &[String], cwd: &str) -> String {
    let program_name = squash(base_name(program));
    let prompt: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    // (acronym, length) ranks; earlier wins ties.
    let mut best: Option<((bool, usize), String)> = None;
    let mut issue: Option<String> = None;
    for arg in &prompt {
        for raw in arg.split(|c: char| !c.is_ascii_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }
            let word = raw.to_ascii_lowercase();
            if raw.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if word.len() < 3 || word == program_name || STOPWORDS.contains(&word.as_str()) {
                continue;
            }
            let acronym = raw.bytes().all(|b| !b.is_ascii_lowercase())
                && raw.bytes().filter(u8::is_ascii_uppercase).count() >= 2;
            let rank = (acronym, word.len().min(MAX_WORD_LEN));
            if best.as_ref().is_none_or(|(r, _)| rank > *r) {
                best = Some((rank, squash(&word)));
            }
        }
        if issue.is_none()
            && let Some(n) = issue_number(arg)
        {
            issue = Some(format!("i{n}"));
        }
    }
    if let Some((_, w)) = best {
        return w;
    }
    if let Some(i) = issue {
        return squash(&i);
    }
    if prompt.is_empty() && SHELLS.contains(&base_name(program)) {
        return "shell".into();
    }
    let dir = squash(base_name(cwd));
    if !dir.is_empty() {
        return dir;
    }
    if !program_name.is_empty() {
        return program_name;
    }
    "agent".into()
}

/// The first `#<digits>` in `s`.
fn issue_number(s: &str) -> Option<&str> {
    s.match_indices('#').find_map(|(i, _)| {
        let rest = &s[i + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        (end > 0).then(|| &rest[..end])
    })
}

/// FNV-1a over the spawn inputs, each ended by a NUL: stable everywhere.
fn spawn_hash(program: &str, args: &[String], cwd: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .chain(std::iter::once(cwd));
    for part in parts {
        for b in part.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The default name of a spawn (ADR 0052): `<task-word>-<animal>`, the
/// animal picked by a stable hash of the inputs, the next one while the
/// name is in `taken` (the live agents' names), and once every animal is
/// taken the first choice with `-2`, `-3`…; at most 32 bytes.
pub fn default_name(program: &str, args: &[String], cwd: &str, taken: &[&str]) -> String {
    let word = task_word(program, args, cwd);
    let start = (spawn_hash(program, args, cwd) % ANIMALS.len() as u64) as usize;
    let free = |n: &str| !taken.contains(&n);
    for k in 0..ANIMALS.len() {
        let name = format!("{word}-{}", ANIMALS[(start + k) % ANIMALS.len()]);
        if free(&name) {
            return name;
        }
    }
    let first = format!("{word}-{}", ANIMALS[start]);
    (2u64..)
        .map(|n| format!("{first}-{n}"))
        .find(|n| free(n))
        .expect("some suffix is free")
}

/// The names of the running agents in `agents`, for [`default_name`].
pub fn live_names(agents: &[AgentInfo]) -> Vec<&str> {
    agents
        .iter()
        .filter(|a| a.state() == AgentState::Running)
        .filter_map(name_of)
        .collect()
}

/// `arg` reads as an agent id (all digits), not a name.
pub fn is_id(arg: &str) -> bool {
    !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit())
}

fn id_list(agents: &[&AgentInfo]) -> String {
    agents
        .iter()
        .map(|a| a.id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolves an id or a name to an agent id (ADR 0052): all digits is an
/// id, taken as it is; a name matches running agents first, else ended
/// ones. No match, or more than one, is an error naming the candidates.
pub fn resolve(agents: &[AgentInfo], arg: &str) -> Result<u64, String> {
    if is_id(arg) {
        return arg.parse().map_err(|_| format!("invalid agent id {arg:?}"));
    }
    let named: Vec<&AgentInfo> = agents.iter().filter(|a| name_of(a) == Some(arg)).collect();
    let live: Vec<&AgentInfo> = named
        .iter()
        .copied()
        .filter(|a| a.state() == AgentState::Running)
        .collect();
    let matches = if live.is_empty() { named } else { live };
    match matches.as_slice() {
        [one] => Ok(one.id),
        [] => {
            let mut similar: Vec<&str> = agents
                .iter()
                .filter_map(name_of)
                .filter(|n| n.contains(arg))
                .collect();
            similar.sort_unstable();
            similar.dedup();
            if similar.is_empty() {
                Err(format!("no agent named {arg:?}"))
            } else {
                Err(format!(
                    "no agent named {arg:?}; similar: {}",
                    similar.join(", ")
                ))
            }
        }
        many => Err(format!(
            "agent name {arg:?} is ambiguous: agents {}; use an id",
            id_list(many)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn agent(id: u64, name: Option<&str>, state: AgentState) -> AgentInfo {
        let mut a = AgentInfo {
            id,
            ..Default::default()
        };
        a.set_state(state);
        if let Some(n) = name {
            a.labels.insert(NAME_LABEL.into(), n.into());
        }
        a
    }

    fn valid(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= MAX_DEFAULT_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !name.starts_with('-')
            && !name.ends_with('-')
    }

    #[test]
    fn task_word_is_the_most_telling_prompt_word() {
        let w = |a: &[&str]| task_word("claude", &args(a), "/w/repo");
        assert_eq!(w(&["fix the SIGTERM race in fleetd serve"]), "sigterm");
        assert_eq!(
            w(&["perf: fat LTO and one codegen unit for release builds"]),
            "lto"
        );
        // No acronym: the longest, then the earliest.
        assert_eq!(w(&["make the badger dance quickly"]), "quickly");
        assert_eq!(w(&["omp skill install"]), "install");
        // Flags are not the prompt; the program's own name is skipped.
        assert_eq!(
            w(&["--dangerously-skip-permissions", "claude: tidy the docs"]),
            "tidy"
        );
        // Boilerplate words are skipped.
        assert_eq!(
            w(&["You are working on GitHub issue #48 in this repo: agent names"]),
            "names"
        );
        // Long words are cut.
        assert_eq!(w(&["supercalifragilisticexpialidocious"]).len(), 16);
    }

    #[test]
    fn task_word_falls_back_to_issue_shell_cwd_then_program() {
        assert_eq!(
            task_word("claude", &args(&["work on issue #23 please"]), "/w"),
            "i23"
        );
        assert_eq!(task_word("/bin/zsh", &args(&["-l"]), "/Users/me"), "shell");
        assert_eq!(task_word("bash", &[], "/x"), "shell");
        assert_eq!(task_word("claude", &[], "/Users/me/fleetd"), "fleetd");
        assert_eq!(task_word("claude", &[], "/Users/me/My Repo!"), "myrepo");
        assert_eq!(task_word("/usr/bin/claude", &[], "/"), "claude");
        assert_eq!(task_word("", &[], ""), "agent");
    }

    #[test]
    fn default_name_is_word_dash_animal_and_deterministic() {
        let a = args(&["fix the SIGTERM race in fleetd serve"]);
        let n = default_name("claude", &a, "/w", &[]);
        assert!(n.starts_with("sigterm-"), "{n}");
        assert!(valid(&n), "{n}");
        assert_eq!(n, default_name("claude", &a, "/w", &[]));
        // Other inputs may pick another animal, but never another shape.
        let m = default_name("claude", &a, "/elsewhere", &[]);
        assert!(m.starts_with("sigterm-") && valid(&m), "{m}");
    }

    #[test]
    fn default_name_changes_the_animal_then_counts_to_stay_unique() {
        let a = args(&["fix the SIGTERM race"]);
        let first = default_name("claude", &a, "/w", &[]);
        let second = default_name("claude", &a, "/w", &[first.as_str()]);
        assert_ne!(first, second);
        assert!(second.starts_with("sigterm-") && valid(&second));
        // Every animal taken: the first choice with -2, then -3.
        let all: Vec<String> = ANIMALS.iter().map(|x| format!("sigterm-{x}")).collect();
        let mut taken: Vec<&str> = all.iter().map(String::as_str).collect();
        let two = default_name("claude", &a, "/w", &taken);
        assert_eq!(two, format!("{first}-2"));
        taken.push(&two);
        assert_eq!(
            default_name("claude", &a, "/w", &taken),
            format!("{first}-3")
        );
    }

    #[test]
    fn default_name_fits_32_bytes_even_with_a_suffix() {
        let a = args(&["supercalifragilisticexpialidocious"]);
        let first = default_name("claude", &a, "/w", &[]);
        assert!(valid(&first), "{first}");
        let all: Vec<String> = ANIMALS
            .iter()
            .map(|x| format!("{}-{x}", task_word("claude", &a, "/w")))
            .collect();
        let mut taken: Vec<&str> = all.iter().map(String::as_str).collect();
        taken.push(&first);
        let n = default_name("claude", &a, "/w", &taken);
        assert!(valid(&n) && !taken.contains(&n.as_str()), "{n}");
    }

    #[test]
    fn animals_are_short_and_distinct() {
        assert!(ANIMALS.len() >= 32);
        for (i, a) in ANIMALS.iter().enumerate() {
            assert!(a.len() <= 7 && a.bytes().all(|b| b.is_ascii_lowercase()));
            assert!(!ANIMALS[..i].contains(a), "{a} twice");
        }
    }

    #[test]
    fn check_name_trims_and_refuses_empty_digits_control_and_long() {
        assert_eq!(check_name("  lto-falcon "), Ok("lto-falcon".into()));
        assert_eq!(check_name("Fix login"), Ok("Fix login".into()));
        assert!(check_name("   ").is_err());
        assert!(check_name("42").is_err());
        assert!(check_name("a\tb").is_err());
        assert!(check_name(&"x".repeat(MAX_NAME_CHARS)).is_ok());
        assert!(check_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn name_of_and_live_names() {
        let agents = [
            agent(1, Some("a-otter"), AgentState::Running),
            agent(2, Some("b-heron"), AgentState::Exited),
            agent(3, None, AgentState::Running),
            agent(4, Some(""), AgentState::Running),
        ];
        assert_eq!(name_of(&agents[0]), Some("a-otter"));
        assert_eq!(name_of(&agents[2]), None);
        assert_eq!(name_of(&agents[3]), None);
        assert_eq!(live_names(&agents), ["a-otter"]);
    }

    #[test]
    fn resolve_takes_ids_and_names_live_first() {
        let agents = [
            agent(1, Some("lto-falcon"), AgentState::Running),
            agent(2, Some("omp-badger"), AgentState::Exited),
            agent(3, Some("omp-badger"), AgentState::Running),
            agent(4, Some("twin"), AgentState::Running),
            agent(5, Some("twin"), AgentState::Running),
            agent(6, Some("old"), AgentState::Exited),
            agent(7, Some("old"), AgentState::Lost),
        ];
        // An id is taken as it is, known or not.
        assert_eq!(resolve(&agents, "99"), Ok(99));
        assert_eq!(resolve(&agents, "lto-falcon"), Ok(1));
        // A live agent wins over an ended one of the same name.
        assert_eq!(resolve(&agents, "omp-badger"), Ok(3));
        let e = resolve(&agents, "twin").unwrap_err();
        assert!(
            e.contains("ambiguous") && e.contains('4') && e.contains('5'),
            "{e}"
        );
        let e = resolve(&agents, "old").unwrap_err();
        assert!(
            e.contains("ambiguous") && e.contains('6') && e.contains('7'),
            "{e}"
        );
        let e = resolve(&agents, "falcon").unwrap_err();
        assert!(
            e.contains("no agent named") && e.contains("lto-falcon"),
            "{e}"
        );
        let e = resolve(&agents, "nobody").unwrap_err();
        assert!(e.contains("no agent named \"nobody\""), "{e}");
    }
}
