//! `[workflows.git-worktree]` in `~/.fleet/config.toml` (ADR 0093), read
//! each time an action runs, with per-repository overrides in
//! `[workflows.git-worktree.repos.<name>]`.

/// The settings for one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Where new worktrees go; `{repo}` is the repository's name, `~` the
    /// user's home, and a relative root is relative to the main checkout.
    pub root: String,
    /// A new branch is this + the slug, unless `branch` is given.
    pub branch_prefix: String,
    /// Fetch the base from its remote before branching (falls back to the
    /// local ref, with a note).
    pub fetch_base: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: "~/.fleet/worktrees/{repo}".into(),
            branch_prefix: "fleet/".into(),
            fetch_base: true,
        }
    }
}

impl Config {
    /// The settings for repository `repo` from a config file's text (`None`:
    /// no file). `Err` names what is wrong.
    pub fn for_repo(text: Option<&str>, repo: &str) -> Result<Config, String> {
        let mut config = Config::default();
        let Some(text) = text else {
            return Ok(config);
        };
        let doc: toml::Table = text
            .parse()
            .map_err(|e| format!("config.toml: {e}"))?;
        let Some(table) = doc
            .get("workflows")
            .and_then(|w| w.get("git-worktree"))
        else {
            return Ok(config);
        };
        let table = table
            .as_table()
            .ok_or("config.toml: [workflows.git-worktree] is not a table")?;
        config.apply(table, "[workflows.git-worktree]")?;
        if let Some(own) = table.get("repos").and_then(|r| r.get(repo)) {
            let own = own.as_table().ok_or_else(|| {
                format!("config.toml: [workflows.git-worktree.repos.{repo}] is not a table")
            })?;
            config.apply(own, &format!("[workflows.git-worktree.repos.{repo}]"))?;
        }
        Ok(config)
    }

    fn apply(&mut self, table: &toml::Table, at: &str) -> Result<(), String> {
        for (key, value) in table {
            let wrong = |what: &str| format!("config.toml: {at} {key} must be {what}");
            match key.as_str() {
                "root" => {
                    let v = value.as_str().filter(|s| !s.is_empty());
                    self.root = v.ok_or_else(|| wrong("a non-empty string"))?.to_owned();
                }
                "branch_prefix" => {
                    let v = value.as_str().ok_or_else(|| wrong("a string"))?;
                    self.branch_prefix = v.to_owned();
                }
                "fetch_base" => {
                    self.fetch_base = value.as_bool().ok_or_else(|| wrong("true or false"))?;
                }
                // Per-repository tables, read by `for_repo`.
                "repos" if at == "[workflows.git-worktree]" => {}
                other => return Err(format!("config.toml: {at} has no setting {other:?}")),
            }
        }
        Ok(())
    }
}

/// The folder new worktrees of a repository go in: `root` with `{repo}` and
/// `~` filled in, relative to `main_checkout` when relative, without `.`
/// or `..`.
pub fn resolve_root(root: &str, repo: &str, main_checkout: &str, home: &str) -> String {
    let root = root.replace("{repo}", repo);
    let full = if root == "~" {
        home.to_owned()
    } else if let Some(rest) = root.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if root.starts_with('/') {
        root
    } else {
        format!("{main_checkout}/{root}")
    };
    normalize(&full)
}

/// `path` with `.` and `..` resolved lexically (absolute in, absolute out).
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_or_no_table_is_the_default() {
        assert_eq!(Config::for_repo(None, "r").unwrap(), Config::default());
        assert_eq!(
            Config::for_repo(Some("[node]\nmax_children = 3\n"), "r").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn the_workflow_table_and_a_repos_table_override_in_order() {
        let text = r#"
[workflows.git-worktree]
root = "../{repo}-wt"
fetch_base = false

[workflows.git-worktree.repos.fleetd]
root = "~/Developer/fleetd-wt"
branch_prefix = ""
"#;
        let other = Config::for_repo(Some(text), "other").unwrap();
        assert_eq!(other.root, "../{repo}-wt");
        assert_eq!(other.branch_prefix, "fleet/");
        assert!(!other.fetch_base);
        let fleetd = Config::for_repo(Some(text), "fleetd").unwrap();
        assert_eq!(fleetd.root, "~/Developer/fleetd-wt");
        assert_eq!(fleetd.branch_prefix, "");
        assert!(!fleetd.fetch_base);
    }

    #[test]
    fn bad_values_are_errors() {
        let e = Config::for_repo(Some("[workflows.git-worktree]\nfetch_base = \"yes\"\n"), "r")
            .unwrap_err();
        assert!(e.contains("fetch_base"), "{e}");
        let e = Config::for_repo(Some("[workflows.git-worktree]\nroots = \"x\"\n"), "r").unwrap_err();
        assert!(e.contains("roots"), "{e}");
        assert!(Config::for_repo(Some("not toml ["), "r").is_err());
        let e = Config::for_repo(Some("[workflows.git-worktree]\nroot = \"\"\n"), "r").unwrap_err();
        assert!(e.contains("root"), "{e}");
    }

    #[test]
    fn roots_resolve_home_repo_and_relative() {
        assert_eq!(
            resolve_root("~/.fleet/worktrees/{repo}", "fleetd", "/src/fleetd", "/Users/u"),
            "/Users/u/.fleet/worktrees/fleetd"
        );
        assert_eq!(
            resolve_root("../{repo}-wt", "fleetd", "/src/fleetd", "/Users/u"),
            "/src/fleetd-wt"
        );
        assert_eq!(resolve_root("/abs/x/", "r", "/m", "/h"), "/abs/x");
        assert_eq!(resolve_root("~", "r", "/m", "/h"), "/h");
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(normalize("/../a"), "/a");
        assert_eq!(normalize("/"), "/");
    }
}
