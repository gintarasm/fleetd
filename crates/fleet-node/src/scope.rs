//! Who is calling, and what an agent may touch (ADR 0072, spec §1).
//!
//! The spawned-by tree comes from the reserved `fleet.parent` label, which
//! only the node writes. An agent acts on its descendants (`D(a)`, exited
//! ones included), its spawns become its children with narrowed labels
//! (ADR 0025), and quotas bound how deep and wide agents may spawn. The
//! user is never scoped and never refused by a quota.

use std::collections::{HashMap, HashSet};
use std::io;

pub type Labels = HashMap<String, String>;

/// The label the node writes on an agent spawned by another agent: the
/// parent's id (a local id now; `node/id` in M6).
pub const PARENT_LABEL: &str = "fleet.parent";

/// The `name` label (ADR 0052): set by the spawner, never inherited.
pub const NAME_LABEL: &str = "name";

/// How a refusal's message starts; the CLI exits 3 for it.
pub const FORBIDDEN_PREFIX: &str = "forbidden:";

/// Who a connection is, fixed at its handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    User,
    Agent(u64),
}

/// A refusal by scope or quota.
pub fn forbidden(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{FORBIDDEN_PREFIX} {what}"),
    )
}

/// Limits on agents' spawns (`[node]` in `<state-dir>/config.toml`),
/// and whether they wait for the user's approval (ADR 0079).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quotas {
    /// Deepest level an agent may spawn into (roots are at depth 0).
    pub max_depth: u32,
    /// Running children per agent.
    pub max_children: u32,
    /// Running agents on the node.
    pub max_agents: u32,
    /// An agent's spawn waits for the user's approval (`spawn_approval`).
    pub spawn_approval: bool,
    /// How long it waits before it is refused (`spawn_approval_timeout`,
    /// seconds, at least 1).
    pub spawn_approval_timeout_secs: u32,
}

/// The default `spawn_approval_timeout`: five minutes.
pub const SPAWN_APPROVAL_TIMEOUT_SECS: u32 = 300;

impl Default for Quotas {
    fn default() -> Self {
        Quotas {
            max_depth: 3,
            max_children: 8,
            max_agents: 30,
            spawn_approval: false,
            spawn_approval_timeout_secs: SPAWN_APPROVAL_TIMEOUT_SECS,
        }
    }
}

impl Quotas {
    /// The quotas in a `config.toml`'s `[node]` table, defaults for keys it
    /// does not set; and a warning per line of that table it could not use.
    ///
    /// A small reader for `key = integer` (and `spawn_approval = bool`)
    /// lines, not a TOML parser: fleetd needs these few keys and nothing
    /// else from the file (the GUI owns the rest of it).
    pub fn from_config(text: &str) -> (Quotas, Vec<String>) {
        let mut quotas = Quotas::default();
        let mut warnings = Vec::new();
        let mut in_node = false;
        for (n, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                in_node = line == "[node]";
                continue;
            }
            if !in_node {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("config.toml line {}: not `key = value`", n + 1));
                continue;
            };
            let (key, value) = (key.trim(), value.trim().replace('_', ""));
            if key == "spawn_approval" {
                match value.as_str() {
                    "true" => quotas.spawn_approval = true,
                    "false" => quotas.spawn_approval = false,
                    _ => warnings.push(format!(
                        "config.toml line {}: [node] spawn_approval must be true or false",
                        n + 1
                    )),
                }
                continue;
            }
            let (slot, min) = match key {
                "max_depth" => (&mut quotas.max_depth, 0),
                "max_children" => (&mut quotas.max_children, 0),
                "max_agents" => (&mut quotas.max_agents, 0),
                "spawn_approval_timeout" => (&mut quotas.spawn_approval_timeout_secs, 1),
                _ => continue,
            };
            match value.parse::<u32>() {
                Ok(v) if v >= min => *slot = v,
                _ => warnings.push(format!(
                    "config.toml line {}: [node] {key} must be an integer of at least {min}",
                    n + 1
                )),
            }
        }
        (quotas, warnings)
    }
}

fn strip_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(before, _)| before)
}

/// The label the GUI marks its terminals with (ADR 0071): `kind=shell`.
pub const KIND_LABEL: &str = "kind";
pub const SHELL_KIND: &str = "shell";

/// Whether these are a root shell's labels: a terminal the user opened
/// (`kind=shell`, no parent). What runs in it is the user (ADR 0072, as
/// amended).
pub fn is_root_shell(labels: &Labels) -> bool {
    labels.get(KIND_LABEL).map(String::as_str) == Some(SHELL_KIND)
        && !labels.contains_key(PARENT_LABEL)
}

/// The parent an agent's labels name, if any.
pub fn parent_of(labels: &Labels) -> Option<u64> {
    labels.get(PARENT_LABEL)?.parse().ok()
}

/// A child's labels (ADR 0025 "narrow", ADR 0052): the parent's labels
/// except `name` and `fleet.*`, plus the requested ones, which may only add
/// keys, and `fleet.parent`. Requested labels have passed the label checks
/// (no `fleet.*`); the count limit is checked again on the result.
pub fn child_labels(parent: u64, parent_labels: &Labels, requested: Labels) -> io::Result<Labels> {
    let mut labels: Labels = parent_labels
        .iter()
        .filter(|(k, _)| k.as_str() != NAME_LABEL && !k.starts_with("fleet."))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (key, value) in requested {
        match labels.get(&key) {
            Some(old) if *old != value => {
                return Err(forbidden(format!(
                    "label {key:?} is inherited from agent {parent} and cannot be changed"
                )));
            }
            _ => {
                labels.insert(key, value);
            }
        }
    }
    labels.insert(PARENT_LABEL.into(), parent.to_string());
    Ok(labels)
}

/// The spawned-by tree of a node's agents.
#[derive(Debug, Default)]
pub struct Tree {
    parent: HashMap<u64, u64>,
    children: HashMap<u64, Vec<u64>>,
    running: HashSet<u64>,
}

/// One agent, as the tree needs it.
pub struct Member<'a> {
    pub id: u64,
    pub labels: &'a Labels,
    pub running: bool,
}

impl Tree {
    /// The tree of `agents`. A parent that was restarted (`replaced`: old
    /// id to its newest successor) is resolved to its successor, so a
    /// restarted agent keeps its children.
    pub fn new<'a>(
        agents: impl IntoIterator<Item = Member<'a>>,
        replaced: &HashMap<u64, u64>,
    ) -> Tree {
        let mut tree = Tree::default();
        for m in agents {
            if m.running {
                tree.running.insert(m.id);
            }
            if let Some(p) = parent_of(m.labels) {
                let p = replaced.get(&p).copied().unwrap_or(p);
                if p != m.id {
                    tree.parent.insert(m.id, p);
                    tree.children.entry(p).or_default().push(m.id);
                }
            }
        }
        tree
    }

    pub fn parent(&self, id: u64) -> Option<u64> {
        self.parent.get(&id).copied()
    }

    /// The ancestors of `id`, nearest first; stops on a cycle (which the
    /// node never writes).
    fn ancestors(&self, id: u64) -> impl Iterator<Item = u64> + '_ {
        let mut seen = HashSet::new();
        let mut at = id;
        std::iter::from_fn(move || {
            let p = self.parent(at)?;
            if !seen.insert(p) || p == id {
                return None;
            }
            at = p;
            Some(p)
        })
    }

    /// The length of `id`'s parent chain: 0 for a root.
    pub fn depth(&self, id: u64) -> u32 {
        u32::try_from(self.ancestors(id).count()).unwrap_or(u32::MAX)
    }

    /// Whether `id` is a (strict) descendant of `a`.
    pub fn is_descendant(&self, a: u64, id: u64) -> bool {
        self.ancestors(id).any(|p| p == a)
    }

    /// `a`'s descendants, not `a` itself.
    pub fn descendants(&self, a: u64) -> HashSet<u64> {
        let mut out = HashSet::new();
        let mut stack = vec![a];
        while let Some(at) = stack.pop() {
            for &c in self.children.get(&at).into_iter().flatten() {
                if c != a && out.insert(c) {
                    stack.push(c);
                }
            }
        }
        out
    }

    pub fn running_children(&self, a: u64) -> usize {
        self.children
            .get(&a)
            .into_iter()
            .flatten()
            .filter(|c| self.running.contains(c))
            .count()
    }

    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Whether `parent` may start one more child under `quotas`; the
    /// refusal's reason (without the `forbidden:` prefix) if not.
    pub fn check_quota(&self, quotas: &Quotas, parent: u64) -> Result<(), String> {
        let depth = self.depth(parent).saturating_add(1);
        if depth > quotas.max_depth {
            return Err(format!(
                "quota max_depth = {}: a child of agent {parent} would be at depth {depth}",
                quotas.max_depth
            ));
        }
        let children = self.running_children(parent);
        if children >= quotas.max_children as usize {
            return Err(format!(
                "quota max_children = {}: agent {parent} has {children} running children",
                quotas.max_children
            ));
        }
        let running = self.running();
        if running >= quotas.max_agents as usize {
            return Err(format!(
                "quota max_agents = {}: {running} agents are running",
                quotas.max_agents
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    /// `(id, parent, running)` rows.
    fn tree(rows: &[(u64, Option<u64>, bool)], replaced: &[(u64, u64)]) -> Tree {
        let owned: Vec<(u64, Labels, bool)> = rows
            .iter()
            .map(|&(id, p, running)| {
                let l = p.map_or_else(Labels::new, |p| {
                    labels(&[(PARENT_LABEL, p.to_string().as_str())])
                });
                (id, l, running)
            })
            .collect();
        Tree::new(
            owned.iter().map(|(id, labels, running)| Member {
                id: *id,
                labels,
                running: *running,
            }),
            &replaced.iter().copied().collect(),
        )
    }

    #[test]
    fn depth_and_descendants_follow_the_parent_label() {
        // 1 -> 2 -> 3 -> 4, 1 -> 5, 6 alone.
        let t = tree(
            &[
                (1, None, true),
                (2, Some(1), true),
                (3, Some(2), false),
                (4, Some(3), true),
                (5, Some(1), true),
                (6, None, true),
            ],
            &[],
        );
        assert_eq!(t.depth(1), 0);
        assert_eq!(t.depth(4), 3);
        assert_eq!(t.descendants(1), HashSet::from([2, 3, 4, 5]));
        assert_eq!(t.descendants(3), HashSet::from([4]));
        assert!(t.descendants(6).is_empty());
        assert!(t.is_descendant(1, 4), "exited 3 still links 4 to 1");
        assert!(!t.is_descendant(1, 1), "not one's own descendant");
        assert!(!t.is_descendant(2, 5), "a sibling's child is not");
        assert!(!t.is_descendant(4, 1), "ancestors are not");
        assert_eq!(t.running_children(1), 2);
        assert_eq!(t.running_children(2), 0, "its child 3 has exited");
        assert_eq!(t.running(), 5);
    }

    #[test]
    fn a_restarted_parent_keeps_its_children() {
        // 2 (child of 1) was restarted as 7.
        let t = tree(
            &[(1, None, true), (7, Some(1), true), (3, Some(2), true)],
            &[(2, 7)],
        );
        assert!(t.is_descendant(7, 3));
        assert!(t.is_descendant(1, 3));
        assert_eq!(t.depth(3), 2);
    }

    #[test]
    fn a_cycle_ends_the_walk() {
        let t = tree(&[(1, Some(2), true), (2, Some(1), true)], &[]);
        assert_eq!(t.depth(1), 1);
        assert!(!t.is_descendant(1, 1));
        assert_eq!(t.descendants(1), HashSet::from([2]));
    }

    #[test]
    fn quotas_refuse_too_deep_too_wide_and_too_many() {
        let q = Quotas::default();
        // A chain 1 -> 2 -> 3 -> 4: 4 is at depth 3, so its child would be
        // the 4th level below the root.
        let chain = tree(
            &[
                (1, None, true),
                (2, Some(1), true),
                (3, Some(2), true),
                (4, Some(3), true),
            ],
            &[],
        );
        assert!(chain.check_quota(&q, 3).is_ok());
        let e = chain.check_quota(&q, 4).unwrap_err();
        assert!(e.contains("max_depth"), "{e}");

        let mut rows = vec![(1, None, true)];
        rows.extend((2..10).map(|id| (id, Some(1), true)));
        let wide = tree(&rows, &[]);
        let e = wide.check_quota(&q, 1).unwrap_err();
        assert!(e.contains("max_children"), "{e}");
        // An exited child frees its slot.
        rows[8].2 = false;
        assert!(tree(&rows, &[]).check_quota(&q, 1).is_ok());

        let many = tree(
            &(1..=30).map(|id| (id, None, true)).collect::<Vec<_>>(),
            &[],
        );
        let e = many.check_quota(&q, 1).unwrap_err();
        assert!(e.contains("max_agents"), "{e}");
    }

    #[test]
    fn child_labels_inherit_and_only_add_keys() {
        let parent = labels(&[
            ("project", "fleetd"),
            ("name", "boss"),
            ("fleet.parent", "1"),
        ]);
        let got = child_labels(2, &parent, labels(&[("role", "rev"), ("name", "kid")])).unwrap();
        assert_eq!(
            got,
            labels(&[
                ("project", "fleetd"),
                ("role", "rev"),
                ("name", "kid"),
                ("fleet.parent", "2"),
            ])
        );
        // The same value again is fine; a different one is not.
        assert!(child_labels(2, &parent, labels(&[("project", "fleetd")])).is_ok());
        let e = child_labels(2, &parent, labels(&[("project", "other")])).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().starts_with(FORBIDDEN_PREFIX), "{e}");
        // The parent's name is not inherited.
        let got = child_labels(2, &parent, Labels::new()).unwrap();
        assert!(!got.contains_key("name"));
    }

    #[test]
    fn config_reads_the_node_table() {
        let (q, w) = Quotas::from_config(
            "[gui]\nmax_depth = 9\n\n[node] # limits\nmax_depth = 2\nmax_children=4 # c\nmax_agents = 1_000\nother = \"x\"\n[gui.x]\nmax_agents = 5\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            q,
            Quotas {
                max_depth: 2,
                max_children: 4,
                max_agents: 1000,
                ..Quotas::default()
            }
        );
        let (q, w) = Quotas::from_config("[node]\nmax_depth = -1\nmax_agents\n");
        assert_eq!(q, Quotas::default());
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(Quotas::from_config("").0, Quotas::default());
    }

    #[test]
    fn config_reads_spawn_approval() {
        assert!(!Quotas::default().spawn_approval);
        assert_eq!(Quotas::default().spawn_approval_timeout_secs, 300);
        let (q, w) = Quotas::from_config(
            "[node]\nspawn_approval = true # ask me\nspawn_approval_timeout = 60\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(q.spawn_approval);
        assert_eq!(q.spawn_approval_timeout_secs, 60);
        assert!(
            !Quotas::from_config("[node]\nspawn_approval = false\n")
                .0
                .spawn_approval
        );
        let (q, w) =
            Quotas::from_config("[node]\nspawn_approval = 1\nspawn_approval_timeout = 0\n");
        assert!(!q.spawn_approval);
        assert_eq!(
            q.spawn_approval_timeout_secs, 300,
            "a zero timeout is unusable"
        );
        assert_eq!(w.len(), 2, "{w:?}");
    }
}
