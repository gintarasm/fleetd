//! `git-worktree.diff` (M4 spec §3): what changed in a worktree, as a
//! `DiffView`, read-only and capped to fit one frame.

use std::collections::HashSet;

use fleet_proto::{DiffView, FileDiff, FileStatus, Hunk, View, view};

use crate::error::ActionError;
use crate::host::{HostResult, ReadFileRequest};
use crate::registry::{ActionCall, ActionOutcome};

use super::git::{Git, Mode};
use super::parse::{parse_numstat, parse_unified_diff, split_nul};
use super::{base_of, branch_of, resolve_repo, target};

/// The empty tree: what a repository with no commits is compared with.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Most lines of context a caller may ask for.
const MAX_CONTEXT: i64 = 10_000;

/// At most this many files with hunks per reply.
pub const MAX_FILES: usize = 2_000;
/// At most this many hunk lines per reply.
pub const MAX_LINES: usize = 50_000;
/// At most this many bytes of hunk text per reply.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// A file listed at all (stats only past [`MAX_FILES`]); past it the view
/// is cut, `files_changed` still counting every file.
pub const MAX_LISTED: usize = 20_000;
/// A file with a NUL in its first 8 KiB is binary (git's rule).
pub const BINARY_PROBE: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub files: usize,
    pub lines: usize,
    pub bytes: usize,
}

impl Caps {
    pub const REPLY: Caps = Caps {
        files: MAX_FILES,
        lines: MAX_LINES,
        bytes: MAX_BYTES,
    };
}

fn hunk_lines(f: &FileDiff) -> usize {
    f.hunks
        .iter()
        .map(|h| {
            if h.body.is_empty() {
                0
            } else {
                h.body.split(|&b| b == b'\n').count()
            }
        })
        .sum()
}

fn hunk_bytes(f: &FileDiff) -> usize {
    f.hunks.iter().map(|h| h.body.len() + h.header.len()).sum()
}

/// Keeps the hunks of files while the reply is under `caps`; from the first
/// file over them on, files keep their stats only (`hunks_omitted`), so the
/// client asks for each with `file`. Whether any hunks were dropped.
pub fn apply_caps(files: &mut [FileDiff], caps: Caps) -> bool {
    let (mut with_hunks, mut lines, mut bytes) = (0, 0, 0);
    let mut over = false;
    let mut dropped = false;
    for f in files.iter_mut() {
        if f.hunks.is_empty() {
            continue;
        }
        if !over {
            let (l, b) = (hunk_lines(f), hunk_bytes(f));
            over = with_hunks + 1 > caps.files || lines + l > caps.lines || bytes + b > caps.bytes;
            if !over {
                with_hunks += 1;
                lines += l;
                bytes += b;
                continue;
            }
        }
        f.hunks = Vec::new();
        f.hunks_omitted = true;
        dropped = true;
    }
    dropped
}

/// An untracked file as an added file: `data` is what was read of it
/// (`size` in all).
pub fn untracked_file(path: &str, data: &[u8], size: u64) -> FileDiff {
    let mut f = FileDiff {
        new_path: path.to_owned(),
        status: FileStatus::FileAdded as i32,
        ..Default::default()
    };
    if data[..data.len().min(BINARY_PROBE)].contains(&0) {
        f.binary = true;
        return f;
    }
    if (data.len() as u64) < size {
        // Not all read: too big for one reply.
        f.hunks_omitted = true;
        return f;
    }
    if data.is_empty() {
        return f;
    }
    let text = data.strip_suffix(b"\n").unwrap_or(data);
    let mut body = Vec::with_capacity(data.len() + data.len() / 16 + 32);
    let mut n = 0u32;
    for line in text.split(|&b| b == b'\n') {
        if n > 0 {
            body.push(b'\n');
        }
        body.push(b'+');
        body.extend_from_slice(line);
        n += 1;
    }
    if !data.ends_with(b"\n") {
        body.extend_from_slice(b"\n\\ No newline at end of file");
    }
    f.additions = n;
    f.hunks.push(Hunk {
        old_start: 0,
        old_lines: 0,
        new_start: 1,
        new_lines: n,
        header: String::new(),
        body,
    });
    f
}

/// The view's totals from its files.
pub fn view(title: String, from_label: String, to_label: String, files: Vec<FileDiff>) -> DiffView {
    DiffView {
        title,
        from_label,
        to_label,
        files_changed: files.len() as u32,
        additions: files.iter().map(|f| u64::from(f.additions)).sum(),
        deletions: files.iter().map(|f| u64::from(f.deletions)).sum(),
        truncated: files.iter().any(|f| f.hunks_omitted),
        files,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Against {
    /// The working tree (untracked included) against the merge base.
    Base,
    /// The working tree (untracked included) against `HEAD`.
    Uncommitted,
    /// The merge base against `HEAD`: commits only.
    Committed,
}

pub async fn run(git: &Git<'_>, call: &ActionCall) -> Result<ActionOutcome, ActionError> {
    let t = target(git, call).await?;
    let wt = t.path.as_str();
    let against = match call.params.string("against").unwrap_or("base") {
        "uncommitted" => Against::Uncommitted,
        "committed" => Against::Committed,
        _ => Against::Base,
    };
    let context = call.params.int("context").unwrap_or(3);
    if !(0..=MAX_CONTEXT).contains(&context) {
        return Err(ActionError::invalid(format!(
            "context is 0 to {MAX_CONTEXT} lines"
        )));
    }
    let file = call.params.string("file").filter(|f| !f.is_empty());
    let head = git
        .line(wt, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .ok();
    let short = |c: &str| c.get(..7).unwrap_or(c).to_owned();

    let (from, from_label) = match against {
        Against::Uncommitted => match &head {
            Some(h) => (h.clone(), format!("HEAD ({})", short(h))),
            None => (EMPTY_TREE.to_owned(), "nothing (no commits)".to_owned()),
        },
        Against::Base | Against::Committed => {
            let repo = resolve_repo(git, wt).await?;
            let base = base_of(git, &t, &repo.main).await?;
            let mb = match &head {
                Some(_) => git
                    .line(wt, &["merge-base", "--end-of-options", &base, "HEAD"])
                    .await
                    .map_err(|e| {
                        ActionError::failed(format!("no merge base of {base} and HEAD ({e})"))
                    })?,
                None => EMPTY_TREE.to_owned(),
            };
            let label = format!("{base} (merge base {})", short(&mb));
            (mb, label)
        }
    };
    let (to, to_label) = match against {
        Against::Committed => (Some("HEAD"), "HEAD"),
        _ => (None, "working tree"),
    };

    let ctx = format!("-U{context}");
    let mut range: Vec<&str> = vec![&from];
    range.extend(to);
    let mut patch_args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-M",
        &ctx,
    ];
    patch_args.extend(&range);
    let mut stat_args = vec!["diff", "--no-relative", "--numstat", "-z", "-M"];
    stat_args.extend(&range);
    if let Some(f) = file {
        patch_args.extend(["--", f]);
        stat_args.extend(["--", f]);
    }
    let out = git.run(wt, &patch_args, Mode::Read).await?;
    if out.exit_code != Some(0) {
        return Err(ActionError::failed(super::git::failure(&patch_args, &out)));
    }
    let mut files = parse_unified_diff(&out.stdout);
    if out.stdout_truncated {
        // Past the node's output cap: the last file may be cut, and later
        // ones are missing. Their stats come from `--numstat`.
        if let Some(last) = files.last_mut() {
            last.hunks.clear();
            last.hunks_omitted = true;
        }
        add_missing(&mut files, &git.ok(wt, &stat_args, Mode::Read).await?);
    }
    if against != Against::Committed {
        untracked(git, wt, file, &mut files).await?;
    }
    files.sort_by(|a, b| display_path(a).cmp(display_path(b)));
    files.truncate(MAX_LISTED.max(1));
    let caps = match file {
        // One file asked for: its hunks whatever their lines, under the bytes.
        Some(_) => Caps {
            lines: usize::MAX,
            ..Caps::REPLY
        },
        None => Caps::REPLY,
    };
    apply_caps(&mut files, caps);
    let name = wt.rsplit('/').next().unwrap_or(wt);
    let title = format!("{name} · {}", branch_of(git, wt).await);
    Ok(ActionOutcome::View(View {
        kind: Some(view::Kind::Diff(super::diff::view(
            title,
            from_label,
            to_label.into(),
            files,
        ))),
    }))
}

fn display_path(f: &FileDiff) -> &str {
    if f.new_path.is_empty() {
        &f.old_path
    } else {
        &f.new_path
    }
}

/// Files `--numstat` lists that the (cut) patch did not reach, with stats
/// only.
fn add_missing(files: &mut Vec<FileDiff>, numstat: &str) {
    let have: HashSet<String> = files.iter().map(|f| display_path(f).to_owned()).collect();
    for n in parse_numstat(numstat.as_bytes()) {
        if have.contains(&n.path) {
            continue;
        }
        files.push(FileDiff {
            status: if n.old_path.is_some() {
                FileStatus::FileRenamed
            } else {
                FileStatus::FileModified
            } as i32,
            old_path: n.old_path.unwrap_or_else(|| n.path.clone()),
            new_path: n.path,
            binary: n.additions.is_none(),
            additions: n.additions.unwrap_or(0),
            deletions: n.deletions.unwrap_or(0),
            hunks_omitted: n.additions.is_some(),
            ..Default::default()
        });
    }
}

/// Untracked files that are not ignored, as added files (read with
/// `ReadFile` while under the file cap; past it, listed with no stats).
async fn untracked(
    git: &Git<'_>,
    wt: &str,
    file: Option<&str>,
    files: &mut Vec<FileDiff>,
) -> HostResult<()> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if let Some(f) = file {
        args.extend(["--", f]);
    }
    let listed = split_nul(git.ok(wt, &args, Mode::Read).await?.as_bytes());
    for (i, path) in listed.into_iter().enumerate() {
        if files.len() >= MAX_LISTED {
            break;
        }
        if i >= MAX_FILES || path.ends_with('/') {
            // A nested repository (`sub/`) or past the cap: listed only.
            files.push(FileDiff {
                new_path: path,
                status: FileStatus::FileAdded as i32,
                hunks_omitted: true,
                ..Default::default()
            });
            continue;
        }
        let read = git
            .host
            .read_file(ReadFileRequest {
                path: format!("{wt}/{path}"),
                offset: 0,
                max_bytes: MAX_BYTES as u64,
            })
            .await;
        files.push(match read {
            Ok(d) => untracked_file(&path, &d.data, d.size),
            Err(ActionError::Cancelled) => return Err(ActionError::Cancelled),
            // Gone since it was listed, or not a file: listed only.
            Err(_) => FileDiff {
                new_path: path,
                status: FileStatus::FileAdded as i32,
                ..Default::default()
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, lines: usize) -> FileDiff {
        let body = vec!["+x"; lines].join("\n").into_bytes();
        FileDiff {
            new_path: path.into(),
            old_path: path.into(),
            status: FileStatus::FileModified as i32,
            additions: lines as u32,
            hunks: vec![Hunk {
                new_start: 1,
                new_lines: lines as u32,
                body,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn under_the_caps_everything_stays() {
        let mut files = vec![file("a", 3), file("b", 4)];
        assert!(!apply_caps(&mut files, Caps::REPLY));
        assert!(
            files
                .iter()
                .all(|f| !f.hunks_omitted && !f.hunks.is_empty())
        );
    }

    #[test]
    fn past_the_line_cap_the_rest_keep_stats_only() {
        let mut files = vec![file("a", 6), file("b", 6), file("c", 1)];
        let caps = Caps {
            files: 10,
            lines: 10,
            bytes: 1 << 20,
        };
        assert!(apply_caps(&mut files, caps));
        assert!(!files[0].hunks_omitted);
        assert!(files[1].hunks_omitted && files[1].hunks.is_empty());
        assert_eq!(files[1].additions, 6, "stats stay");
        assert!(files[2].hunks_omitted, "the rest too, though it would fit");
    }

    #[test]
    fn the_file_and_byte_caps() {
        let mut files = vec![file("a", 1), file("b", 1), file("c", 1)];
        let caps = Caps {
            files: 2,
            lines: 100,
            bytes: 1 << 20,
        };
        assert!(apply_caps(&mut files, caps));
        assert!(!files[1].hunks_omitted && files[2].hunks_omitted);
        let mut files = vec![file("a", 100)];
        let caps = Caps {
            files: 10,
            lines: 1000,
            bytes: 50,
        };
        assert!(apply_caps(&mut files, caps));
        assert!(files[0].hunks_omitted);
    }

    #[test]
    fn files_without_hunks_are_never_omitted() {
        let mut files = vec![
            file("a", 20),
            FileDiff {
                new_path: "bin".into(),
                binary: true,
                ..Default::default()
            },
        ];
        let caps = Caps {
            files: 10,
            lines: 5,
            bytes: 1 << 20,
        };
        assert!(apply_caps(&mut files, caps));
        assert!(!files[1].hunks_omitted);
    }

    #[test]
    fn an_untracked_text_file_is_one_added_hunk() {
        let f = untracked_file("n.txt", b"a\nb\n", 4);
        assert_eq!(f.status(), FileStatus::FileAdded);
        assert_eq!((f.old_path.as_str(), f.new_path.as_str()), ("", "n.txt"));
        assert_eq!(f.additions, 2);
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (0, 0, 1, 2)
        );
        assert_eq!(h.body, b"+a\n+b".to_vec());
        let f = untracked_file("n", b"a\nb", 3);
        assert_eq!(
            f.hunks[0].body,
            b"+a\n+b\n\\ No newline at end of file".to_vec()
        );
        let f = untracked_file("empty", b"", 0);
        assert!(f.hunks.is_empty() && f.additions == 0);
    }

    #[test]
    fn an_untracked_binary_or_cut_file() {
        let f = untracked_file("b", b"x\0y", 3);
        assert!(f.binary && f.hunks.is_empty() && f.additions == 0);
        let f = untracked_file("big", b"a\nb\n", 1 << 30);
        assert!(f.hunks_omitted && f.hunks.is_empty());
    }

    #[test]
    fn the_views_totals() {
        let mut a = file("a", 2);
        a.deletions = 1;
        let v = view("t".into(), "f".into(), "w".into(), vec![a, file("b", 3)]);
        assert_eq!((v.files_changed, v.additions, v.deletions), (2, 5, 1));
        assert!(!v.truncated);
        let mut c = file("c", 1);
        c.hunks.clear();
        c.hunks_omitted = true;
        assert!(view(String::new(), String::new(), String::new(), vec![c]).truncated);
    }
}
