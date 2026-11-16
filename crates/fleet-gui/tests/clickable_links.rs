//! Clickable URLs and file paths (issue #37): what is a link, and the
//! command that opens a file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use fleet_gui::core::links::{Files, Link, LinkTarget, Osc8, editor_command, link_at, pick_editor};
use fleet_term::Cell;

fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell {
            ch,
            ..Cell::default()
        })
        .collect()
}

/// The link under `col` of `text`, with no OSC 8 link and no files.
fn url_at(text: &str, col: u16) -> Option<(u16, u16, String)> {
    match link_at(&cells(text), col, None, None)? {
        Link {
            start,
            end,
            target: LinkTarget::Url(u),
        } => Some((start, end, u)),
        other => panic!("not a URL: {other:?}"),
    }
}

#[test]
fn a_url_is_found_under_any_of_its_columns() {
    let text = "PR: https://github.com/o/r/pull/37 opened";
    let want = Some((4, 34, "https://github.com/o/r/pull/37".to_owned()));
    assert_eq!(url_at(text, 4), want);
    assert_eq!(url_at(text, 20), want);
    assert_eq!(url_at(text, 33), want);
    assert_eq!(url_at(text, 34), None);
    assert_eq!(url_at(text, 3), None);
    assert_eq!(url_at("see http://x.io", 6).unwrap().2, "http://x.io");
}

#[test]
fn trailing_punctuation_and_unbalanced_brackets_are_not_part_of_a_url() {
    for (text, url) in [
        ("go to https://a.io/x.", "https://a.io/x"),
        ("(https://a.io/x)", "https://a.io/x"),
        ("[pr](https://a.io/p/1),", "https://a.io/p/1"),
        ("<https://a.io/x>", "https://a.io/x"),
        ("\"https://a.io/x\"", "https://a.io/x"),
        (
            "https://en.wikipedia.org/wiki/Rust_(language)",
            "https://en.wikipedia.org/wiki/Rust_(language)",
        ),
        ("https://a.io/?q=1&r=2#f:", "https://a.io/?q=1&r=2#f"),
    ] {
        let col = text.find("https").unwrap() as u16 + 9;
        assert_eq!(
            url_at(text, col).map(|l| l.2).as_deref(),
            Some(url),
            "{text}"
        );
    }
    // A scheme alone is no link.
    assert_eq!(url_at("https:// nothing", 2), None);
    assert_eq!(url_at("ftp://a.io/x", 3), None);
}

#[test]
fn columns_count_wide_cells_twice() {
    let mut row = cells("界 https://a.io");
    row[0].wide = true;
    let link = link_at(&row, 5, None, None).unwrap();
    // 界 takes columns 0 and 1, the space 2, the URL 3..15.
    assert_eq!((link.start, link.end), (3, 15));
    assert!(link_at(&row, 1, None, None).is_none());
}

struct Fs {
    files: HashSet<PathBuf>,
}

impl Fs {
    fn new(files: &[&str]) -> Self {
        Fs {
            files: files.iter().map(PathBuf::from).collect(),
        }
    }

    fn is_file(&self, p: &Path) -> bool {
        self.files.contains(p)
    }
}

/// A file link: its columns, path, line and column.
type FileLink = (u16, u16, PathBuf, Option<u32>, Option<u32>);

fn file_at(text: &str, col: u16, fs: &Fs) -> Option<FileLink> {
    let is_file = |p: &Path| fs.is_file(p);
    let files = Files {
        cwd: Path::new("/work"),
        home: Some(Path::new("/home/me")),
        is_file: &is_file,
    };
    match link_at(&cells(text), col, None, Some(&files))? {
        Link {
            start,
            end,
            target: LinkTarget::File { path, line, col },
        } => Some((start, end, path, line, col)),
        other => panic!("not a file: {other:?}"),
    }
}

#[test]
fn an_existing_path_relative_to_the_cwd_is_a_file_link_with_its_line_and_column() {
    let fs = Fs::new(&[
        "/work/src/main.rs",
        "/etc/hosts",
        "/home/me/notes.md",
        "/work/Cargo.toml",
    ]);
    let p = |s: &str| PathBuf::from(s);
    assert_eq!(
        file_at("  --> src/main.rs:10:5", 9, &fs),
        Some((6, 22, p("/work/src/main.rs"), Some(10), Some(5)))
    );
    assert_eq!(
        file_at("edit src/main.rs:42.", 6, &fs),
        Some((5, 19, p("/work/src/main.rs"), Some(42), None))
    );
    assert_eq!(
        file_at("see `src/main.rs`", 6, &fs),
        Some((5, 16, p("/work/src/main.rs"), None, None))
    );
    assert_eq!(
        file_at("read /etc/hosts", 7, &fs).map(|l| l.2),
        Some(p("/etc/hosts"))
    );
    assert_eq!(
        file_at("in ~/notes.md:3", 5, &fs).map(|l| (l.2, l.3)),
        Some((p("/home/me/notes.md"), Some(3)))
    );
    assert_eq!(
        file_at("./Cargo.toml", 3, &fs).map(|l| l.2),
        Some(p("/work/Cargo.toml"))
    );
    // A bare file name with an extension.
    assert_eq!(
        file_at("Cargo.toml updated", 2, &fs).map(|l| (l.0, l.1)),
        Some((0, 10))
    );
    // git diff's a/ and b/ prefixes.
    assert_eq!(
        file_at("+++ b/src/main.rs", 8, &fs).map(|l| (l.0, l.2)),
        Some((4, p("/work/src/main.rs")))
    );
}

#[test]
fn only_files_that_exist_are_linked() {
    let fs = Fs::new(&["/work/src/main.rs", "/work/src"]);
    assert_eq!(file_at("src/lib.rs:3", 3, &fs), None);
    assert_eq!(file_at("version 1.5 out", 9, &fs), None);
    // A word that is no path.
    assert_eq!(file_at("hello world", 2, &fs), None);
    // Remote agents: no files at all.
    assert!(link_at(&cells("src/main.rs"), 2, None, None).is_none());
}

#[test]
fn an_osc8_hyperlink_wins_over_the_text_under_it() {
    let row = cells("click here for the PR");
    let osc8 = Osc8 {
        uri: "https://a.io/pr/1",
        start: 6,
        end: 10,
    };
    let link = link_at(&row, 7, Some(osc8), None).unwrap();
    assert_eq!(
        (link.start, link.end, link.target),
        (6, 10, LinkTarget::Url("https://a.io/pr/1".into()))
    );
    // Schemes other than http(s) and mailto are not opened.
    let osc8 = Osc8 {
        uri: "x-evil://run",
        start: 6,
        end: 10,
    };
    assert!(link_at(&row, 7, Some(osc8), None).is_none());
    let osc8 = Osc8 {
        uri: "mailto:me@a.io",
        start: 6,
        end: 10,
    };
    assert!(matches!(
        link_at(&row, 7, Some(osc8), None).unwrap().target,
        LinkTarget::Url(_)
    ));
}

#[test]
fn an_osc8_file_uri_is_a_file_link_when_it_exists() {
    let fs = Fs::new(&["/work/a b.rs"]);
    let is_file = |p: &Path| fs.is_file(p);
    let files = Files {
        cwd: Path::new("/work"),
        home: None,
        is_file: &is_file,
    };
    let row = cells("ab");
    let at = |uri| Osc8 {
        uri,
        start: 0,
        end: 2,
    };
    let link = link_at(&row, 0, Some(at("file://host/work/a%20b.rs")), Some(&files)).unwrap();
    assert_eq!(
        link.target,
        LinkTarget::File {
            path: "/work/a b.rs".into(),
            line: None,
            col: None
        }
    );
    assert!(link_at(&row, 0, Some(at("file:///work/gone.rs")), Some(&files)).is_none());
    assert!(link_at(&row, 0, Some(at("file:///work/a%20b.rs")), None).is_none());
}

#[test]
fn the_editor_is_the_setting_then_visual_then_editor() {
    assert_eq!(
        pick_editor("zed", Some("code"), Some("vim")).as_deref(),
        Some("zed")
    );
    assert_eq!(
        pick_editor(" ", Some("code -w"), Some("vim")).as_deref(),
        Some("code -w")
    );
    assert_eq!(pick_editor("", None, Some("subl")).as_deref(), Some("subl"));
    assert_eq!(pick_editor("", Some(""), None), None);
}

#[test]
fn editors_open_the_file_at_its_line_and_column() {
    let path = Path::new("/w/a.rs");
    let cmd = |e: Option<&str>, l, c| editor_command(e, path, l, c);
    assert_eq!(
        cmd(Some("code"), Some(3), Some(4)),
        ["code", "-g", "/w/a.rs:3:4"]
    );
    assert_eq!(
        cmd(Some("/usr/local/bin/cursor -r"), Some(3), None),
        ["/usr/local/bin/cursor", "-r", "-g", "/w/a.rs:3"]
    );
    assert_eq!(cmd(Some("zed"), Some(3), Some(4)), ["zed", "/w/a.rs:3:4"]);
    assert_eq!(cmd(Some("subl"), Some(3), None), ["subl", "/w/a.rs:3"]);
    assert_eq!(
        cmd(Some("mate"), Some(3), None),
        ["mate", "-l", "3", "/w/a.rs"]
    );
    assert_eq!(
        cmd(Some("rustrover"), Some(3), Some(4)),
        ["rustrover", "--line", "3", "/w/a.rs"]
    );
    assert_eq!(
        cmd(Some("emacsclient -n"), Some(3), Some(4)),
        ["emacsclient", "-n", "+3:4", "/w/a.rs"]
    );
    assert_eq!(
        cmd(Some("bbedit"), Some(3), None),
        ["bbedit", "+3", "/w/a.rs"]
    );
    // No line: just the file.
    assert_eq!(cmd(Some("code"), None, None), ["code", "/w/a.rs"]);
    // An unknown editor gets the file only.
    assert_eq!(
        cmd(Some("myedit --x"), Some(3), None),
        ["myedit", "--x", "/w/a.rs"]
    );
}

#[test]
fn without_a_gui_editor_the_file_opens_in_the_default_text_editor() {
    let path = Path::new("/w/a.rs");
    let open = ["open", "-t", "/w/a.rs"];
    assert_eq!(editor_command(None, path, Some(3), None), open);
    // Terminal editors need a terminal the GUI does not give them.
    for e in ["vim", "/usr/bin/nvim", "nano", "hx", "emacs -nw", "vi"] {
        assert_eq!(editor_command(Some(e), path, Some(3), None), open, "{e}");
    }
    // A command line that does not parse.
    assert_eq!(editor_command(Some("code 'oops"), path, None, None), open);
}
