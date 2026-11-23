//! What a workflow declares it uses (ADR 0007's plugin capabilities, ADR
//! 0092): the host holds every workflow to it, built-ins too.

/// One capability, as a manifest writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capability {
    /// `exec:<bin>`: run `bin` (exactly as `argv[0]`, looked up on the
    /// node's `PATH`); `exec:*` runs anything.
    Exec(String),
    /// `fs:read:<prefix>`: read files and list directories under an
    /// absolute path prefix; `fs:read:*` anywhere.
    FsRead(String),
    /// `spawn`: start agents (as the caller) and forget them.
    Spawn,
    /// `labels:<prefix>`: the label prefix the workflow owns (at most one).
    Labels(String),
    /// `storage:own`: the workflow's own key-value storage.
    StorageOwn,
}

/// Whether `prefix` may be an owned label prefix: the node's rule
/// (`fleet_node::scope::check_owned_prefix`), checked here too so a bad
/// manifest fails when the registry is built.
pub fn valid_label_prefix(prefix: &str) -> bool {
    prefix.len() >= 2
        && prefix.len() <= 32
        && prefix.ends_with('.')
        && !prefix.starts_with('.')
        && !prefix.contains("..")
        && !prefix.starts_with("fleet.")
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `path` is absolute with no `.` or `..` components (so a prefix
/// check on it means what it says).
pub fn plain_absolute(path: &str) -> bool {
    path.starts_with('/') && !path.split('/').any(|c| c == ".." || c == ".")
}

impl Capability {
    pub fn parse(s: &str) -> Result<Capability, String> {
        let bad = || Err(format!("unknown or malformed capability {s:?}"));
        if s == "spawn" {
            return Ok(Capability::Spawn);
        }
        if s == "storage:own" {
            return Ok(Capability::StorageOwn);
        }
        if let Some(bin) = s.strip_prefix("exec:") {
            if bin.is_empty() || bin.contains('/') {
                return bad();
            }
            return Ok(Capability::Exec(bin.into()));
        }
        if let Some(prefix) = s.strip_prefix("fs:read:") {
            if prefix != "*" && !plain_absolute(prefix) {
                return bad();
            }
            return Ok(Capability::FsRead(prefix.into()));
        }
        if let Some(prefix) = s.strip_prefix("labels:") {
            if !valid_label_prefix(prefix) {
                return bad();
            }
            return Ok(Capability::Labels(prefix.into()));
        }
        bad()
    }
}

/// A workflow's declared capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    capabilities: Vec<Capability>,
}

impl Manifest {
    /// A manifest from capability strings; `Err` names the first bad one.
    pub fn parse(capabilities: &[&str]) -> Result<Manifest, String> {
        let capabilities = capabilities
            .iter()
            .map(|c| Capability::parse(c))
            .collect::<Result<Vec<_>, _>>()?;
        let prefixes = capabilities
            .iter()
            .filter(|c| matches!(c, Capability::Labels(_)))
            .count();
        if prefixes > 1 {
            return Err("a workflow owns at most one label prefix".into());
        }
        Ok(Manifest { capabilities })
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Whether `argv0` may be run.
    pub fn allows_exec(&self, argv0: &str) -> bool {
        !argv0.is_empty()
            && self
                .capabilities
                .iter()
                .any(|c| matches!(c, Capability::Exec(bin) if bin == "*" || bin == argv0))
    }

    /// Whether `path` may be read or listed.
    pub fn allows_read(&self, path: &str) -> bool {
        plain_absolute(path)
            && self.capabilities.iter().any(|c| match c {
                Capability::FsRead(p) if p == "*" => true,
                Capability::FsRead(p) => {
                    let p = p.trim_end_matches('/');
                    path == p
                        || path
                            .strip_prefix(p)
                            .is_some_and(|rest| rest.starts_with('/'))
                }
                _ => false,
            })
    }

    pub fn allows_spawn(&self) -> bool {
        self.capabilities.contains(&Capability::Spawn)
    }

    pub fn allows_storage(&self) -> bool {
        self.capabilities.contains(&Capability::StorageOwn)
    }

    /// The label prefix the workflow owns.
    pub fn owned_prefix(&self) -> Option<&str> {
        self.capabilities.iter().find_map(|c| match c {
            Capability::Labels(p) => Some(p.as_str()),
            _ => None,
        })
    }

    /// Whether the workflow may write label `key`.
    pub fn allows_label(&self, key: &str) -> bool {
        self.owned_prefix().is_some_and(|p| key.starts_with(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(caps: &[&str]) -> Manifest {
        Manifest::parse(caps).unwrap()
    }

    #[test]
    fn capabilities_parse() {
        assert_eq!(
            Capability::parse("exec:git"),
            Ok(Capability::Exec("git".into()))
        );
        assert_eq!(
            Capability::parse("exec:*"),
            Ok(Capability::Exec("*".into()))
        );
        assert_eq!(
            Capability::parse("fs:read:/home/x"),
            Ok(Capability::FsRead("/home/x".into()))
        );
        assert_eq!(
            Capability::parse("fs:read:*"),
            Ok(Capability::FsRead("*".into()))
        );
        assert_eq!(Capability::parse("spawn"), Ok(Capability::Spawn));
        assert_eq!(
            Capability::parse("labels:git."),
            Ok(Capability::Labels("git.".into()))
        );
        assert_eq!(Capability::parse("storage:own"), Ok(Capability::StorageOwn));
        for bad in [
            "",
            "exec:",
            "exec:a/b",
            "fs:read:",
            "fs:read:rel",
            "fs:write:/x",
            "labels:git",
            "labels:fleet.",
            "labels:",
            "storage:all",
            "net",
        ] {
            assert!(Capability::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_manifest_has_at_most_one_label_prefix() {
        assert!(Manifest::parse(&["labels:a.", "labels:b."]).is_err());
        assert_eq!(manifest(&["labels:git."]).owned_prefix(), Some("git."));
        assert_eq!(manifest(&["spawn"]).owned_prefix(), None);
    }

    #[test]
    fn exec_allows_the_named_program_only() {
        let m = manifest(&["exec:git"]);
        assert!(m.allows_exec("git"));
        assert!(!m.allows_exec("/usr/bin/git"), "a path is not the bin");
        assert!(!m.allows_exec("sh"));
        assert!(!m.allows_exec(""));
        assert!(manifest(&["exec:*"]).allows_exec("/bin/sh"));
        assert!(!manifest(&[]).allows_exec("git"));
    }

    #[test]
    fn reads_stay_under_the_prefix() {
        let m = manifest(&["fs:read:/home/u/repo"]);
        assert!(m.allows_read("/home/u/repo"));
        assert!(m.allows_read("/home/u/repo/src/a.rs"));
        assert!(!m.allows_read("/home/u/repository"), "component-wise");
        assert!(!m.allows_read("/home/u/repo/../secret"), "no ..");
        assert!(!m.allows_read("relative/path"));
        let any = manifest(&["fs:read:*"]);
        assert!(any.allows_read("/etc/hosts"));
        assert!(!any.allows_read("/a/../b"), "no .. even for *");
        assert!(!any.allows_read("rel"));
        assert!(!manifest(&[]).allows_read("/x"));
    }

    #[test]
    fn spawn_storage_and_labels_are_flags() {
        let m = manifest(&["spawn", "storage:own", "labels:git."]);
        assert!(m.allows_spawn() && m.allows_storage());
        assert!(m.allows_label("git.repo"));
        assert!(!m.allows_label("git"));
        assert!(!m.allows_label("project"));
        let none = manifest(&[]);
        assert!(!none.allows_spawn() && !none.allows_storage());
        assert!(!none.allows_label("git.repo"));
    }
}
