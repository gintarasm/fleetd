//! The diff pane's model (ADR 0094): a [`DiffView`] from
//! `git-worktree.diff` turned into display rows once per refresh, so the
//! pane draws only the rows in its viewport.

use fleet_proto::{DiffView, FileDiff, FileStatus};

use super::session::PaneId;
use super::tabs::Tabs;

/// A line longer than this is cut, with an ellipsis (no horizontal scroll
/// in M4).
pub const MAX_LINE_CHARS: usize = 2_000;

/// What a display row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A file's header: status, path, +/−. Space collapses it.
    File,
    /// `@@ -a,b +c,d @@ header`.
    Hunk,
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`.
    NoNewline,
    /// A one-line note: binary, omitted hunks, mode change, no changes.
    Note,
}

/// One display row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// The file it belongs to (index into [`DiffDoc::files`]).
    pub file: usize,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// What is drawn, without the `+`/`-`/` ` marker, cut at
    /// [`MAX_LINE_CHARS`].
    pub text: String,
}

/// One entry of the file list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// `A`, `M`, `D`, `R`, `C`, `T`.
    pub status: char,
    /// The new path, or the old one for a deleted file; `old → new` for a
    /// rename or copy.
    pub path: String,
    /// The path to ask for with the `file` param.
    pub key: String,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
    /// Over the reply's cap: no hunks; expanding it asks for this file.
    pub omitted: bool,
    pub collapsed: bool,
}

/// A diff laid out for display.
#[derive(Clone, Debug, Default)]
pub struct DiffDoc {
    pub title: String,
    pub from_label: String,
    pub to_label: String,
    pub files_changed: u32,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
    files: Vec<FileEntry>,
    /// Every file's rows, header first, in file order.
    rows: Vec<Row>,
    /// Each file's range in `rows`.
    spans: Vec<(usize, usize)>,
    /// The rows shown (collapsed files show their header only).
    visible: Vec<u32>,
}

impl DiffDoc {
    pub fn new(view: &DiffView) -> DiffDoc {
        let mut doc = DiffDoc {
            title: view.title.clone(),
            from_label: view.from_label.clone(),
            to_label: view.to_label.clone(),
            files_changed: view.files_changed,
            additions: view.additions,
            deletions: view.deletions,
            truncated: view.truncated,
            ..DiffDoc::default()
        };
        for (i, f) in view.files.iter().enumerate() {
            doc.files.push(entry(f));
            let from = doc.rows.len();
            push_rows(&mut doc.rows, i, f);
            doc.spans.push((from, doc.rows.len()));
        }
        doc.relayout();
        doc
    }

    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// How many rows are shown.
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// The `i`th shown row.
    pub fn row(&self, i: usize) -> Option<&Row> {
        let at = *self.visible.get(i)?;
        self.rows.get(at as usize)
    }

    /// Collapses or expands file `file`. `true` when an omitted file was
    /// expanded: its hunks are to be asked for (`file` param).
    pub fn toggle(&mut self, file: usize) -> bool {
        let Some(f) = self.files.get_mut(file) else {
            return false;
        };
        f.collapsed = !f.collapsed;
        let ask = !f.collapsed && f.omitted;
        if ask {
            // It stays collapsed until its hunks arrive.
            f.collapsed = true;
        }
        self.relayout();
        ask
    }

    /// Puts the hunks of one file, asked for on its own, in place of
    /// `file`'s, expanded.
    pub fn replace_file(&mut self, file: usize, diff: &FileDiff) {
        if file >= self.files.len() {
            return;
        }
        let mut rows = Vec::new();
        push_rows(&mut rows, file, diff);
        let (from, to) = self.spans[file];
        let grown = rows.len() as isize - (to - from) as isize;
        self.rows.splice(from..to, rows);
        self.spans[file].1 = (to as isize + grown) as usize;
        for span in &mut self.spans[file + 1..] {
            span.0 = (span.0 as isize + grown) as usize;
            span.1 = (span.1 as isize + grown) as usize;
        }
        let mut e = entry(diff);
        e.collapsed = false;
        e.omitted = diff.hunks_omitted;
        self.files[file] = e;
        self.relayout();
    }

    /// The shown row of `file`'s header.
    pub fn header_row(&self, file: usize) -> Option<usize> {
        let start = self.spans.get(file)?.0 as u32;
        self.visible.binary_search(&start).ok()
    }

    /// The file the shown row `i` belongs to (the sticky header's).
    pub fn file_at(&self, i: usize) -> Option<usize> {
        self.row(i).map(|r| r.file)
    }

    /// The shown rows `from..=to` (either order) as text, as a unified
    /// diff writes them.
    pub fn copy_text(&self, from: usize, to: usize) -> String {
        let (a, b) = if from <= to { (from, to) } else { (to, from) };
        let mut out = String::new();
        for i in a..=b {
            let Some(r) = self.row(i) else { break };
            match r.kind {
                RowKind::Context => out.push(' '),
                RowKind::Added => out.push('+'),
                RowKind::Removed => out.push('-'),
                RowKind::NoNewline => out.push_str("\\ "),
                _ => {}
            }
            out.push_str(&r.text);
            out.push('\n');
        }
        out
    }

    fn relayout(&mut self) {
        self.visible.clear();
        for (f, &(from, to)) in self.files.iter().zip(&self.spans) {
            let to = if f.collapsed { from + 1 } else { to };
            self.visible.extend((from..to).map(|i| i as u32));
        }
    }
}

fn entry(f: &FileDiff) -> FileEntry {
    let status = status(f.status());
    let key = if f.new_path.is_empty() {
        f.old_path.clone()
    } else {
        f.new_path.clone()
    };
    let path = if matches!(status, 'R' | 'C') && !f.old_path.is_empty() && f.old_path != key {
        format!("{} \u{2192} {key}", f.old_path)
    } else {
        key.clone()
    };
    FileEntry {
        status,
        path,
        key,
        additions: f.additions,
        deletions: f.deletions,
        binary: f.binary,
        omitted: f.hunks_omitted,
        collapsed: f.hunks_omitted,
    }
}

fn push_rows(rows: &mut Vec<Row>, file: usize, f: &FileDiff) {
    let e = entry(f);
    let note = |rows: &mut Vec<Row>, text: String| {
        rows.push(Row {
            kind: RowKind::Note,
            file,
            old: None,
            new: None,
            text,
        });
    };
    rows.push(Row {
        kind: RowKind::File,
        file,
        old: None,
        new: None,
        text: e.path,
    });
    if !f.old_mode.is_empty() && !f.new_mode.is_empty() && f.old_mode != f.new_mode {
        note(
            rows,
            format!("Mode changed {} \u{2192} {}", f.old_mode, f.new_mode),
        );
    }
    if f.binary {
        note(rows, "Binary file not shown".into());
        return;
    }
    if f.hunks_omitted {
        // Asked for on expand; collapsed until then.
        return;
    }
    for h in &f.hunks {
        let mut head = format!(
            "@@ -{},{} +{},{} @@",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        );
        if !h.header.is_empty() {
            head.push(' ');
            head.push_str(&h.header);
        }
        rows.push(Row {
            kind: RowKind::Hunk,
            file,
            old: None,
            new: None,
            text: head,
        });
        let (mut old, mut new) = (h.old_start, h.new_start);
        let body = String::from_utf8_lossy(&h.body);
        let body = body.strip_suffix('\n').unwrap_or(&body);
        if body.is_empty() {
            continue;
        }
        for line in body.split('\n') {
            let mut chars = line.chars();
            let marker = chars.next().unwrap_or(' ');
            let rest = chars.as_str();
            let (kind, o, n, text) = match marker {
                '+' => (RowKind::Added, None, Some(new), rest),
                '-' => (RowKind::Removed, Some(old), None, rest),
                '\\' => (RowKind::NoNewline, None, None, rest.trim_start()),
                ' ' => (RowKind::Context, Some(old), Some(new), rest),
                _ => (RowKind::Context, Some(old), Some(new), line),
            };
            if o.is_some() {
                old += 1;
            }
            if n.is_some() {
                new += 1;
            }
            rows.push(Row {
                kind,
                file,
                old: o,
                new: n,
                text: cut(text),
            });
        }
    }
}

/// `text`, cut at [`MAX_LINE_CHARS`] with an ellipsis.
fn cut(text: &str) -> String {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => {
            let mut s = String::with_capacity(at + 3);
            s.push_str(&text[..at]);
            s.push('\u{2026}');
            s
        }
        None => text.to_owned(),
    }
}

/// `tabs` without `panes` (the diff panes), as saved: a diff pane is not
/// restored after a relaunch. Which tab is shown, and each tab's focus
/// where it was not a removed pane, stay.
pub fn without_panes(tabs: &Tabs, panes: &[PaneId]) -> Tabs {
    let mut t = tabs.clone();
    let active = t.active();
    for &pane in panes {
        let Some(tab) = t.tab_of(pane) else { continue };
        t.select(tab);
        let keep = t.focused();
        t.focus(pane);
        t.close_focused();
        if keep != pane {
            t.focus(keep);
        }
    }
    t.select(active);
    t
}

/// A table as aligned text lines (header first, columns two spaces
/// apart), as `fleet action run` prints one.
pub fn table_lines(t: &fleet_proto::TableView) -> Vec<String> {
    let n = t
        .columns
        .len()
        .max(t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0));
    let header: Vec<String> = t.columns.iter().map(|c| c.to_uppercase()).collect();
    let rows: Vec<Vec<&str>> = std::iter::once(&header)
        .chain(t.rows.iter().map(|r| &r.cells))
        .map(|cells| {
            (0..n)
                .map(|i| cells.get(i).map_or("", String::as_str))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..n)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (i, cell) in r.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                line.push_str(cell);
                let pad = widths[i] - cell.chars().count();
                line.extend(std::iter::repeat_n(' ', pad));
            }
            cut(line.trim_end())
        })
        .collect()
}

/// A text view's lines, each cut at [`MAX_LINE_CHARS`].
pub fn text_lines(text: &str) -> Vec<String> {
    text.lines().map(cut).collect()
}

/// A fixture diff of `lines` changed lines over `files` modified files,
/// in hunks of 50 (the GUI benchmarks, ADR 0094).
pub fn fixture(lines: usize, files: usize) -> DiffView {
    let files = files.max(1);
    let per_file = lines.div_ceil(files);
    let mut out = Vec::new();
    let mut left = lines;
    for f in 0..files {
        let n = per_file.min(left);
        left -= n;
        let mut hunks = Vec::new();
        let mut done = 0;
        while done < n {
            let len = 50.min(n - done);
            let start = (done * 2 + 1) as u32;
            let body: String = (0..len)
                .map(|i| match i % 5 {
                    0 => format!("-let old_{i} = compute({i}, \"before\");\n"),
                    1 => format!("+let new_{i} = compute({i}, \"after\");\n"),
                    _ => format!(" let same_{i} = keep({i});\n"),
                })
                .collect();
            let removed = (0..len).filter(|i| i % 5 == 0).count() as u32;
            let added = (0..len).filter(|i| i % 5 == 1).count() as u32;
            hunks.push(fleet_proto::Hunk {
                old_start: start,
                old_lines: len as u32 - added,
                new_start: start,
                new_lines: len as u32 - removed,
                header: format!("fn part_{done}()"),
                body: body.into_bytes(),
            });
            done += len;
        }
        out.push(FileDiff {
            old_path: format!("src/file_{f}.rs"),
            new_path: format!("src/file_{f}.rs"),
            status: FileStatus::FileModified as i32,
            additions: (n / 5) as u32,
            deletions: (n / 5) as u32,
            hunks,
            ..Default::default()
        });
    }
    DiffView {
        title: "fixture · fleet/bench".into(),
        from_label: "origin/main".into(),
        to_label: "working tree".into(),
        files_changed: out.len() as u32,
        additions: (lines / 5) as u64,
        deletions: (lines / 5) as u64,
        files: out,
        truncated: false,
    }
}

/// `+` / `-` and the counts, for headers: `+12 −3`.
pub fn stat_text(additions: u64, deletions: u64) -> String {
    format!("+{additions} \u{2212}{deletions}")
}

/// A file status as `git status --short` writes it.
pub fn status(s: FileStatus) -> char {
    match s {
        FileStatus::FileAdded => 'A',
        FileStatus::FileDeleted => 'D',
        FileStatus::FileRenamed => 'R',
        FileStatus::FileCopied => 'C',
        FileStatus::FileTypeChanged => 'T',
        FileStatus::FileModified | FileStatus::Unspecified => 'M',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::Hunk;

    fn hunk(old: (u32, u32), new: (u32, u32), header: &str, body: &str) -> Hunk {
        Hunk {
            old_start: old.0,
            old_lines: old.1,
            new_start: new.0,
            new_lines: new.1,
            header: header.into(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn modified(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            old_path: path.into(),
            new_path: path.into(),
            status: FileStatus::FileModified as i32,
            additions: 1,
            deletions: 1,
            hunks,
            ..Default::default()
        }
    }

    fn view(files: Vec<FileDiff>) -> DiffView {
        DiffView {
            title: "wt · feat/x".into(),
            from_label: "origin/main".into(),
            to_label: "working tree".into(),
            files_changed: files.len() as u32,
            additions: 1,
            deletions: 1,
            files,
            truncated: false,
        }
    }

    fn kinds(d: &DiffDoc) -> Vec<RowKind> {
        (0..d.len()).map(|i| d.row(i).unwrap().kind).collect()
    }

    #[test]
    fn lines_get_old_and_new_numbers() {
        let body = " a\n-b\n+B\n c\n";
        let d = DiffDoc::new(&view(vec![modified(
            "src/x.rs",
            vec![hunk((10, 3), (10, 3), "fn x()", body)],
        )]));
        use RowKind::*;
        assert_eq!(kinds(&d), [File, Hunk, Context, Removed, Added, Context]);
        let nums: Vec<_> = (2..6)
            .map(|i| {
                let r = d.row(i).unwrap();
                (r.old, r.new, r.text.as_str())
            })
            .collect();
        assert_eq!(
            nums,
            [
                (Some(10), Some(10), "a"),
                (Some(11), None, "b"),
                (None, Some(11), "B"),
                (Some(12), Some(12), "c"),
            ]
        );
        assert_eq!(d.row(1).unwrap().text, "@@ -10,3 +10,3 @@ fn x()");
        assert_eq!(d.row(0).unwrap().text, "src/x.rs");
    }

    #[test]
    fn no_newline_marker_has_no_numbers() {
        let body = "-a\n\\ No newline at end of file\n+a\n";
        let d = DiffDoc::new(&view(vec![modified(
            "f",
            vec![hunk((1, 1), (1, 1), "", body)],
        )]));
        let r = d.row(3).unwrap();
        assert_eq!(r.kind, RowKind::NoNewline);
        assert_eq!((r.old, r.new), (None, None));
        assert_eq!(r.text, "No newline at end of file");
        assert_eq!(d.row(4).unwrap().new, Some(1));
    }

    #[test]
    fn long_lines_are_cut_with_an_ellipsis() {
        let long = "é".repeat(MAX_LINE_CHARS + 50);
        let body = format!("+{long}\n");
        let d = DiffDoc::new(&view(vec![modified(
            "f",
            vec![hunk((0, 0), (1, 1), "", &body)],
        )]));
        let text = &d.row(2).unwrap().text;
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
        assert!(text.ends_with('\u{2026}'));
    }

    #[test]
    fn binary_omitted_and_mode_changes_get_a_note() {
        let binary = FileDiff {
            new_path: "img.png".into(),
            status: FileStatus::FileAdded as i32,
            binary: true,
            ..Default::default()
        };
        let omitted = FileDiff {
            hunks_omitted: true,
            additions: 9000,
            ..modified("big.rs", vec![])
        };
        let mode = FileDiff {
            old_mode: "100644".into(),
            new_mode: "100755".into(),
            additions: 0,
            deletions: 0,
            ..modified("run.sh", vec![])
        };
        let d = DiffDoc::new(&view(vec![binary, omitted, mode]));
        let notes: Vec<_> = (0..d.len())
            .filter_map(|i| d.row(i))
            .filter(|r| r.kind == RowKind::Note)
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(
            notes,
            [
                "Binary file not shown".to_owned(),
                "Mode changed 100644 \u{2192} 100755".to_owned(),
            ]
        );
        // An omitted file starts collapsed: its hunks load on expand.
        assert!(d.files()[1].omitted && d.files()[1].collapsed);
        assert_eq!(d.files()[0].status, 'A');
        assert_eq!(d.files()[0].path, "img.png");
    }

    #[test]
    fn renames_show_both_paths_and_deletions_the_old_one() {
        let renamed = FileDiff {
            old_path: "a.rs".into(),
            new_path: "b.rs".into(),
            status: FileStatus::FileRenamed as i32,
            ..Default::default()
        };
        let deleted = FileDiff {
            old_path: "gone.rs".into(),
            status: FileStatus::FileDeleted as i32,
            ..Default::default()
        };
        let d = DiffDoc::new(&view(vec![renamed, deleted]));
        assert_eq!(d.files()[0].path, "a.rs \u{2192} b.rs");
        assert_eq!(d.files()[0].status, 'R');
        assert_eq!(d.files()[0].key, "b.rs");
        assert_eq!(d.files()[1].path, "gone.rs");
        assert_eq!(d.files()[1].key, "gone.rs");
        assert_eq!(d.files()[1].status, 'D');
    }

    #[test]
    fn collapsing_hides_a_files_rows_but_not_its_header() {
        let d0 = view(vec![
            modified("a", vec![hunk((1, 1), (1, 1), "", "-x\n+y\n")]),
            modified("b", vec![hunk((1, 1), (1, 1), "", "-x\n+y\n")]),
        ]);
        let mut d = DiffDoc::new(&d0);
        assert_eq!(d.len(), 8);
        assert!(!d.toggle(0));
        assert_eq!(d.len(), 5);
        assert!(d.files()[0].collapsed);
        assert_eq!(d.header_row(1), Some(1));
        assert_eq!(d.row(1).unwrap().text, "b");
        d.toggle(0);
        assert_eq!(d.len(), 8);
        assert_eq!(d.header_row(1), Some(4));
    }

    #[test]
    fn sticky_header_follows_the_file_of_a_row() {
        let d = DiffDoc::new(&view(vec![
            modified("a", vec![hunk((1, 1), (1, 1), "", "-x\n+y\n")]),
            modified("b", vec![hunk((1, 1), (1, 1), "", "-x\n+y\n")]),
        ]));
        assert_eq!(d.file_at(0), Some(0));
        assert_eq!(d.file_at(3), Some(0));
        assert_eq!(d.file_at(4), Some(1));
        assert_eq!(d.file_at(99), None);
    }

    #[test]
    fn expanding_an_omitted_file_asks_for_it_and_its_reply_fills_it() {
        let omitted = FileDiff {
            hunks_omitted: true,
            ..modified("big.rs", vec![])
        };
        let mut d = DiffDoc::new(&view(vec![omitted]));
        assert_eq!(d.len(), 1);
        assert!(d.toggle(0), "an omitted file is asked for");
        let full = modified("big.rs", vec![hunk((1, 1), (1, 1), "", "-x\n+y\n")]);
        d.replace_file(0, &full);
        assert!(!d.files()[0].omitted && !d.files()[0].collapsed);
        assert_eq!(d.len(), 4);
        // Collapsing and expanding it again asks for nothing.
        d.toggle(0);
        assert!(!d.toggle(0));
    }

    #[test]
    fn copy_text_writes_lines_as_a_unified_diff() {
        let d = DiffDoc::new(&view(vec![modified(
            "f",
            vec![hunk((1, 2), (1, 2), "", " a\n-b\n+c\n")],
        )]));
        assert_eq!(d.copy_text(1, 4), "@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
        assert_eq!(d.copy_text(4, 2), " a\n-b\n+c\n");
    }

    #[test]
    fn saved_tabs_leave_diff_panes_out() {
        use crate::core::layout::Dir;
        let mut tabs = Tabs::single();
        let first = tabs.focused();
        let diff = tabs.split_toward(first, Dir::Right).unwrap();
        tabs.set_agent(first, Some(7));
        let second_tab = tabs.new_tab().unwrap();
        tabs.select(0);
        tabs.focus(first);
        let saved = without_panes(&tabs, &[diff]);
        assert_eq!(saved.panes(), [first, second_tab]);
        assert_eq!(saved.active(), 0);
        assert_eq!(saved.focused(), first);
        assert_eq!(saved.agent(first), Some(7));
        // Nothing to leave out: the same tabs.
        assert_eq!(without_panes(&tabs, &[]), tabs);
    }

    #[test]
    fn tables_align_their_columns() {
        use fleet_proto::{TableRow, TableView};
        let t = TableView {
            title: "worktrees".into(),
            columns: vec!["path".into(), "branch".into()],
            rows: vec![
                TableRow {
                    cells: vec!["/wt/a".into(), "fleet/a".into()],
                    ..Default::default()
                },
                TableRow {
                    cells: vec!["/wt/long-name".into(), "b".into()],
                    ..Default::default()
                },
            ],
        };
        assert_eq!(
            table_lines(&t),
            [
                "PATH           BRANCH",
                "/wt/a          fleet/a",
                "/wt/long-name  b",
            ]
        );
        assert_eq!(text_lines("a\nb\n"), ["a", "b"]);
    }

    #[test]
    fn the_benchmark_fixture_has_its_lines() {
        let v = fixture(10_000, 20);
        assert_eq!(v.files.len(), 20);
        let d = DiffDoc::new(&v);
        // Every line, plus a header per file and per hunk of 50.
        assert_eq!(d.len(), 10_000 + 20 + 200);
    }

    #[test]
    fn empty_view_has_no_rows() {
        let d = DiffDoc::new(&view(vec![]));
        assert!(d.is_empty());
        assert_eq!(d.file_at(0), None);
    }

    #[test]
    fn a_ten_thousand_line_diff_builds_every_row() {
        let body: String = (0..10_000).map(|i| format!("+line {i}\n")).collect();
        let d = DiffDoc::new(&view(vec![modified(
            "f",
            vec![hunk((0, 0), (1, 10_000), "", &body)],
        )]));
        assert_eq!(d.len(), 10_002);
        assert_eq!(d.row(10_001).unwrap().new, Some(10_000));
    }
}
