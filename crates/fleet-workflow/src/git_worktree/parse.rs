//! Pure parsers of git's output (ADR 0093): `worktree list --porcelain -z`,
//! `diff --numstat -z`, NUL-separated path lists, and unified diffs into
//! [`FileDiff`]s. No I/O: unit-tested on fixtures.

use fleet_proto::{FileDiff, FileStatus, Hunk};

/// One entry of `git worktree list --porcelain -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: String,
    pub head: String,
    /// The branch's short name (`fleet/x`), `None` when detached or bare.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    /// Its folder is gone (`git worktree prune` would drop it).
    pub prunable: bool,
}

pub fn parse_worktree_list(out: &[u8]) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for field in out.split(|&b| b == 0) {
        let field = String::from_utf8_lossy(field);
        if field.is_empty() {
            entries.extend(cur.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((&field, ""));
        if key == "worktree" {
            entries.extend(cur.take());
            cur = Some(WorktreeEntry {
                path: value.to_owned(),
                ..Default::default()
            });
            continue;
        }
        let Some(e) = cur.as_mut() else { continue };
        match key {
            "HEAD" => e.head = value.to_owned(),
            "branch" => {
                e.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                )
            }
            "detached" => e.detached = true,
            "bare" => e.bare = true,
            "locked" => e.locked = true,
            "prunable" => e.prunable = true,
            _ => {}
        }
    }
    entries.extend(cur);
    entries
}

/// One entry of `git diff --numstat -z`: `None` counts for a binary file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumStat {
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    /// Set for a rename or copy.
    pub old_path: Option<String>,
    pub path: String,
}

pub fn parse_numstat(out: &[u8]) -> Vec<NumStat> {
    let mut fields = out.split(|&b| b == 0);
    let mut stats = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let field = String::from_utf8_lossy(field);
        let mut parts = field.splitn(3, '\t');
        let (Some(add), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (old_path, path) = if path.is_empty() {
            // A rename or copy: the two paths follow as fields of their own.
            let old = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned());
            let new = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned());
            (old, new.unwrap_or_default())
        } else {
            (None, path.to_owned())
        };
        stats.push(NumStat {
            additions: add.parse().ok(),
            deletions: del.parse().ok(),
            old_path,
            path,
        });
    }
    stats
}

/// The paths of a `-z` list (`ls-files -z`, `status -z` paths).
pub fn split_nul(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect()
}

/// Parses `git diff` output (`--no-color --no-ext-diff`, `core.quotepath=off`)
/// into one [`FileDiff`] per file, hunks included, stats counted.
pub fn parse_unified_diff(patch: &[u8]) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // The hunk being read: its body grows line by line.
    let mut in_hunk = false;
    for line in patch.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            finish(files.last_mut());
            let (old, new) = git_header_paths(&String::from_utf8_lossy(rest));
            files.push(FileDiff {
                old_path: old,
                new_path: new,
                status: FileStatus::FileModified as i32,
                ..Default::default()
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with(b"@@ ") {
            if let Some(h) = hunk_header(&String::from_utf8_lossy(line)) {
                file.hunks.push(h);
                in_hunk = true;
            }
            continue;
        }
        if in_hunk {
            match line.first() {
                Some(b'+' | b'-' | b' ' | b'\\') => {
                    match line[0] {
                        b'+' => file.additions += 1,
                        b'-' => file.deletions += 1,
                        _ => {}
                    }
                    let body = &mut file.hunks.last_mut().expect("in a hunk").body;
                    if !body.is_empty() {
                        body.push(b'\n');
                    }
                    body.extend_from_slice(line);
                    continue;
                }
                // An empty line ends the patch; anything else, the hunk.
                _ => in_hunk = false,
            }
        }
        extended_header(file, &String::from_utf8_lossy(line));
    }
    finish(files.last_mut());
    files
}

/// Settles a file once all its lines are read: added and deleted files
/// lose the path they do not have.
fn finish(file: Option<&mut FileDiff>) {
    let Some(f) = file else { return };
    match f.status() {
        FileStatus::FileAdded => f.old_path.clear(),
        FileStatus::FileDeleted => f.new_path.clear(),
        _ => {}
    }
    if !f.old_mode.is_empty()
        && !f.new_mode.is_empty()
        && file_type(&f.old_mode) != file_type(&f.new_mode)
    {
        f.status = FileStatus::FileTypeChanged as i32;
    }
}

/// The object type of a git mode (`100644` → `10`, a symlink's `120000` →
/// `12`).
fn file_type(mode: &str) -> &str {
    mode.get(..2).unwrap_or(mode)
}

/// A line between `diff --git` and the first hunk.
fn extended_header(f: &mut FileDiff, line: &str) {
    let set = |s: &mut String, v: &str| *s = unquote(v.trim_end_matches('\t'));
    if let Some(m) = line.strip_prefix("new file mode ") {
        f.status = FileStatus::FileAdded as i32;
        f.new_mode = m.to_owned();
    } else if let Some(m) = line.strip_prefix("deleted file mode ") {
        f.status = FileStatus::FileDeleted as i32;
        f.old_mode = m.to_owned();
    } else if let Some(m) = line.strip_prefix("old mode ") {
        f.old_mode = m.to_owned();
    } else if let Some(m) = line.strip_prefix("new mode ") {
        f.new_mode = m.to_owned();
    } else if let Some(p) = line.strip_prefix("rename from ") {
        f.status = FileStatus::FileRenamed as i32;
        set(&mut f.old_path, p);
    } else if let Some(p) = line.strip_prefix("rename to ") {
        set(&mut f.new_path, p);
    } else if let Some(p) = line.strip_prefix("copy from ") {
        f.status = FileStatus::FileCopied as i32;
        set(&mut f.old_path, p);
    } else if let Some(p) = line.strip_prefix("copy to ") {
        set(&mut f.new_path, p);
    } else if let Some(rest) = line.strip_prefix("index ") {
        // `index abc..def 100644`: the mode of an unchanged-mode file.
        if let Some((_, mode)) = rest.split_once(' ')
            && f.old_mode.is_empty()
            && f.new_mode.is_empty()
        {
            f.old_mode = mode.to_owned();
            f.new_mode = mode.to_owned();
        }
    } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
        f.binary = true;
    } else if let Some(p) = line.strip_prefix("--- ") {
        if let Some(p) = strip_side(p, "a/") {
            set(&mut f.old_path, &p);
        }
    } else if let Some(p) = line.strip_prefix("+++ ")
        && let Some(p) = strip_side(p, "b/")
    {
        set(&mut f.new_path, &p);
    }
}

/// `a/path` (maybe quoted) → `path`; `/dev/null` → `None`.
fn strip_side(p: &str, side: &str) -> Option<String> {
    let p = p.trim_end_matches('\t');
    if p == "/dev/null" {
        return None;
    }
    let unq = unquote(p);
    unq.strip_prefix(side).map(str::to_owned)
}

/// The two paths of `diff --git a/X b/Y` (after `diff --git `). Unquoted
/// paths with spaces are ambiguous; they are split in the middle when both
/// sides are the same path, and fixed later by `---`/`+++` or rename lines.
fn git_header_paths(rest: &str) -> (String, String) {
    let (a, b) = if rest.starts_with('"') {
        let (a, after) = take_quoted(rest);
        (a, unquote(after.trim_start()))
    } else if let Some(i) = rest.find(" \"b/") {
        (rest[..i].to_owned(), unquote(&rest[i + 1..]))
    } else {
        let n = rest.len();
        let mid = n.saturating_sub(1) / 2;
        if n % 2 == 1 && rest.is_char_boundary(mid) && rest[mid + 1..].starts_with("b/") {
            (rest[..mid].to_owned(), rest[mid + 1..].to_owned())
        } else {
            match rest.split_once(" b/") {
                Some((a, b)) => (a.to_owned(), format!("b/{b}")),
                None => (rest.to_owned(), String::new()),
            }
        }
    };
    let a = a.strip_prefix("a/").unwrap_or(&a).to_owned();
    let b = b.strip_prefix("b/").unwrap_or(&b).to_owned();
    (a, b)
}

/// A C-quoted string at the start of `s` and what follows it.
fn take_quoted(s: &str) -> (String, &str) {
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return (unquote(&s[..=i]), &s[i + 1..]),
            _ => i += 1,
        }
    }
    (unquote(s), "")
}

/// Undoes git's C-style quoting of a path (`"tab\t\"q\".txt"`); an
/// unquoted path is returned as is.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut out = Vec::with_capacity(inner.len());
    let b = inner.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        i += 2;
        out.push(match c {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'v' => 0x0b,
            b'0'..=b'7' => {
                // Three octal digits: a raw byte (UTF-8 is quoted this way
                // without core.quotepath=off).
                let digits = &b[i - 1..(i + 2).min(b.len())];
                i = (i + 2).min(b.len());
                digits.iter().fold(0u8, |acc, d| {
                    acc.wrapping_mul(8).wrapping_add(d.wrapping_sub(b'0'))
                })
            }
            other => other,
        });
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `@@ -a,b +c,d @@ context` → a hunk with no body yet.
fn hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, header) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((s, n)) => Some((s.parse().ok()?, n.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        header: header.strip_prefix(' ').unwrap_or(header).to_owned(),
        body: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &[u8] = include_bytes!("fixtures/basic.diff");

    fn by_path<'a>(files: &'a [FileDiff], path: &str) -> &'a FileDiff {
        files
            .iter()
            .find(|f| f.new_path == path || (f.new_path.is_empty() && f.old_path == path))
            .unwrap_or_else(|| panic!("no {path:?} in {files:#?}"))
    }

    #[test]
    fn worktree_list_porcelain() {
        let got = parse_worktree_list(include_bytes!("fixtures/worktrees.porcelain"));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].path, "/R/r");
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert_eq!(got[0].head, "618d387fe2116ac8f61e2bf502857bcbef2685fd");
        assert!(got[1].detached && got[1].branch.is_none());
        assert_eq!(got[1].path, "/R/wt-d");
        assert_eq!(got[2].branch.as_deref(), Some("fleet/x"));
        assert!(!got[2].locked && !got[2].prunable);
    }

    #[test]
    fn worktree_list_flags() {
        let out = b"worktree /b\0bare\0\0worktree /w\0HEAD 1\0branch refs/heads/a\0locked reason\0prunable gitdir file points to non-existent location\0\0";
        let got = parse_worktree_list(out);
        assert!(got[0].bare);
        assert!(got[1].locked && got[1].prunable);
    }

    #[test]
    fn numstat_with_renames_and_binaries() {
        let got = parse_numstat(include_bytes!("fixtures/basic.numstat"));
        assert_eq!(got.len(), 9);
        assert_eq!(
            got[1],
            NumStat {
                additions: None,
                deletions: None,
                old_path: None,
                path: "bin.dat".into()
            }
        );
        let ren = got.iter().find(|n| n.path == "ren_new.txt").unwrap();
        assert_eq!(ren.old_path.as_deref(), Some("ren_old.txt"));
        assert_eq!((ren.additions, ren.deletions), (Some(1), Some(0)));
        assert!(got.iter().any(|n| n.path == "tab\t\"q\".txt"));
    }

    #[test]
    fn nul_lists() {
        assert_eq!(split_nul(b"a\0b c\0"), vec!["a", "b c"]);
        assert!(split_nul(b"").is_empty());
    }

    #[test]
    fn unified_diff_statuses_and_paths() {
        let files = parse_unified_diff(BASIC);
        assert_eq!(files.len(), 9, "{files:#?}");
        let added = by_path(&files, "added.txt");
        assert_eq!(added.status(), FileStatus::FileAdded);
        assert_eq!(added.old_path, "");
        assert_eq!(added.new_mode, "100644");
        assert_eq!((added.additions, added.deletions), (2, 0));
        let del = by_path(&files, "del.txt");
        assert_eq!(del.status(), FileStatus::FileDeleted);
        assert_eq!(del.new_path, "");
        assert_eq!(del.old_mode, "100644");
        assert_eq!(del.deletions, 1);
        let bin = by_path(&files, "bin.dat");
        assert!(bin.binary && bin.hunks.is_empty());
        assert_eq!(bin.status(), FileStatus::FileModified);
        let exec = by_path(&files, "exec.sh");
        assert_eq!(
            (exec.old_mode.as_str(), exec.new_mode.as_str()),
            ("100644", "100755")
        );
        assert!(exec.hunks.is_empty());
        let ren = by_path(&files, "ren_new.txt");
        assert_eq!(ren.status(), FileStatus::FileRenamed);
        assert_eq!(ren.old_path, "ren_old.txt");
        assert_eq!(
            by_path(&files, "sp ace.txt").status(),
            FileStatus::FileModified
        );
        let quoted = by_path(&files, "tab\t\"q\".txt");
        assert_eq!(quoted.old_path, "tab\t\"q\".txt");
    }

    #[test]
    fn unified_diff_hunks() {
        let files = parse_unified_diff(BASIC);
        let m = by_path(&files, "mod.txt");
        assert_eq!((m.additions, m.deletions), (2, 1));
        assert_eq!(m.hunks.len(), 1);
        let h = &m.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (1, 7, 1, 8)
        );
        assert_eq!(
            String::from_utf8_lossy(&h.body),
            " one\n-two\n+TWO\n three\n four\n five\n six\n seven\n+EIGHT"
        );
        let n = by_path(&files, "nonl.txt");
        let h = &n.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (1, 1, 1, 2)
        );
        assert_eq!(
            String::from_utf8_lossy(&h.body),
            "-end\n\\ No newline at end of file\n+end\n+more\n\\ No newline at end of file"
        );
        let ren = by_path(&files, "ren_new.txt");
        assert_eq!(ren.hunks[0].old_start, 38);
        let del = by_path(&files, "del.txt");
        assert_eq!((del.hunks[0].new_start, del.hunks[0].new_lines), (0, 0));
    }

    #[test]
    fn hunk_headers_keep_the_function_context() {
        let patch = b"diff --git a/f.rs b/f.rs\nindex 1..2 100644\n--- a/f.rs\n+++ b/f.rs\n@@ -10,2 +10,2 @@ fn main() {\n-a\n+b\n x\n";
        let files = parse_unified_diff(patch);
        assert_eq!(files[0].hunks[0].header, "fn main() {");
        assert_eq!(files[0].hunks[0].body, b"-a\n+b\n x".to_vec());
    }

    #[test]
    fn empty_and_garbage_input() {
        assert!(parse_unified_diff(b"").is_empty());
        assert!(parse_unified_diff(b"not a diff\n").is_empty());
    }

    #[test]
    fn a_cut_off_diff_keeps_what_it_has() {
        let cut = &BASIC[..BASIC.len() / 2];
        let files = parse_unified_diff(cut);
        assert!(!files.is_empty());
    }
}
