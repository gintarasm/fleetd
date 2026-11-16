//! Clickable links in a pane (ADR 0046): URLs, OSC 8 hyperlinks and file
//! paths found in a row's cells, and the command that opens a file in the
//! user's editor.
//!
//! Detection works on one row and one column at a time, the one under the
//! pointer, so a path costs one existence check per pointer move.

use std::path::{Path, PathBuf};

use fleet_term::Cell;

/// What a link opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// Opened in the browser (or mail client, for `mailto:`).
    Url(String),
    /// Opened in the editor, at `line` and `col` when the text gave them.
    File {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// A link on a row: columns `start..end` and what it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub start: u16,
    pub end: u16,
    pub target: LinkTarget,
}

/// An OSC 8 hyperlink under the pointer: its URI and columns `start..end`.
#[derive(Clone, Copy, Debug)]
pub struct Osc8<'a> {
    pub uri: &'a str,
    pub start: u16,
    pub end: u16,
}

/// Where file paths are looked up: the agent's directory, the home for
/// `~/`, and which paths are existing files. Only for a local agent: a
/// remote agent's paths are not on this machine.
pub struct Files<'a> {
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
}

/// URL schemes the text and OSC 8 links may open.
const URL_SCHEMES: [&str; 2] = ["https://", "http://"];

/// The link under column `col` of a row. An OSC 8 hyperlink there wins;
/// else a URL in the text, else a path to an existing file (only with
/// `files`).
pub fn link_at(
    cells: &[Cell],
    col: u16,
    osc8: Option<Osc8<'_>>,
    files: Option<&Files>,
) -> Option<Link> {
    if let Some(o) = osc8 {
        return uri_target(o.uri, files).map(|target| Link {
            start: o.start,
            end: o.end,
            target,
        });
    }
    let row = Row::new(cells);
    let at = row.index_at(col)?;
    let (start, end) = token(&row.chars, at)?;
    if let Some((s, e)) = url_in(&row.chars[start..end], at - start) {
        let url: String = row.chars[start + s..start + e].iter().collect();
        return Some(Link {
            start: row.cols[start + s],
            end: row.end_col(start + e - 1),
            target: LinkTarget::Url(url),
        });
    }
    let files = files?;
    let (s, e) = trim_path(&row.chars[start..end]);
    if at < start + s || at >= start + e {
        return None;
    }
    let text: String = row.chars[start + s..start + e].iter().collect();
    let target = path_target(&text, files)?;
    Some(Link {
        start: row.cols[start + s],
        end: row.end_col(start + e - 1),
        target,
    })
}

/// A row's characters and the column each starts at.
struct Row {
    chars: Vec<char>,
    cols: Vec<u16>,
    widths: Vec<u16>,
}

impl Row {
    fn new(cells: &[Cell]) -> Self {
        let mut row = Row {
            chars: Vec::with_capacity(cells.len()),
            cols: Vec::with_capacity(cells.len()),
            widths: Vec::with_capacity(cells.len()),
        };
        let mut col = 0u16;
        for c in cells {
            let w = if c.wide { 2 } else { 1 };
            row.chars.push(c.ch);
            row.cols.push(col);
            row.widths.push(w);
            col = col.saturating_add(w);
        }
        row
    }

    /// The character covering column `col`.
    fn index_at(&self, col: u16) -> Option<usize> {
        (0..self.chars.len()).find(|&i| self.cols[i] <= col && col < self.cols[i] + self.widths[i])
    }

    /// The column after character `i`.
    fn end_col(&self, i: usize) -> u16 {
        self.cols[i] + self.widths[i]
    }
}

/// Characters that end a link on either side.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '`' | '\'' | '│' | '|')
}

/// The run of non-boundary characters around `at`.
fn token(chars: &[char], at: usize) -> Option<(usize, usize)> {
    if is_boundary(chars[at]) {
        return None;
    }
    let start = chars[..at]
        .iter()
        .rposition(|&c| is_boundary(c))
        .map_or(0, |i| i + 1);
    let end = chars[at..]
        .iter()
        .position(|&c| is_boundary(c))
        .map_or(chars.len(), |i| at + i);
    Some((start, end))
}

/// The URL in `token` covering `at`, as `start..end` within it.
fn url_in(token: &[char], at: usize) -> Option<(usize, usize)> {
    let text: String = token.iter().collect();
    // Byte offsets in `text` to char offsets: schemes are ASCII, but what
    // precedes them may not be.
    let start = URL_SCHEMES
        .iter()
        .filter_map(|s| text.find(s).map(|b| (b, s.len())))
        .min()?;
    let (byte, scheme_len) = start;
    let s = text[..byte].chars().count();
    let mut e = token.len();
    while e > s {
        let last = token[e - 1];
        let url = &token[s..e];
        let unbalanced = |open: char, close: char| {
            last == close
                && url.iter().filter(|&&c| c == open).count()
                    < url.iter().filter(|&&c| c == close).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '*')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
            || unbalanced('{', '}')
        {
            e -= 1;
        } else {
            break;
        }
    }
    (e > s + scheme_len && (s..e).contains(&at)).then_some((s, e))
}

/// A path token without the brackets and punctuation around it, as
/// `start..end` within it.
fn trim_path(token: &[char]) -> (usize, usize) {
    let lead = |c: char| matches!(c, '(' | '[' | '{');
    let trail = |c: char| matches!(c, ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?');
    let s = token.iter().position(|&c| !lead(c)).unwrap_or(token.len());
    let e = token
        .iter()
        .rposition(|&c| !trail(c))
        .map_or(s, |i| i + 1)
        .max(s);
    (s, e)
}

/// `text` as `path[:line[:col]]` naming an existing file.
fn path_target(text: &str, files: &Files) -> Option<LinkTarget> {
    let (path, line, col) = split_line_col(text);
    if !(path.contains('/') || path.contains('.')) || path.chars().all(|c| c == '.' || c == '/') {
        return None;
    }
    let candidates = [Some(path), path.strip_prefix("a/"), path.strip_prefix("b/")];
    candidates.into_iter().flatten().find_map(|p| {
        let full = resolve(p, files)?;
        (files.is_file)(&full).then_some(LinkTarget::File {
            path: full,
            line,
            col,
        })
    })
}

/// Splits a trailing `:line` or `:line:col` off `text`.
fn split_line_col(text: &str) -> (&str, Option<u32>, Option<u32>) {
    let number = |s: &str| -> Option<(usize, u32)> {
        let i = s.rfind(':')?;
        let n = &s[i + 1..];
        (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .then(|| n.parse().ok().map(|v| (i, v)))?
    };
    match number(text) {
        Some((i, last)) => match number(&text[..i]) {
            Some((j, first)) => (&text[..j], Some(first), Some(last)),
            None => (&text[..i], Some(last), None),
        },
        None => (text, None, None),
    }
}

fn resolve(path: &str, files: &Files) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        return files.home.map(|h| h.join(rest));
    }
    let p = Path::new(path);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        files.cwd.join(p)
    })
}

/// What an OSC 8 URI opens: http(s) and mailto as URLs, `file:` URIs as
/// existing files; nothing for other schemes.
fn uri_target(uri: &str, files: Option<&Files>) -> Option<LinkTarget> {
    if URL_SCHEMES.iter().any(|s| uri.starts_with(s)) || uri.starts_with("mailto:") {
        return Some(LinkTarget::Url(uri.to_owned()));
    }
    let files = files?;
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path`: the host is dropped, the agent is local.
    let path = percent_decode(&rest[rest.find('/')?..])?;
    let path = PathBuf::from(path);
    (files.is_file)(&path).then_some(LinkTarget::File {
        path,
        line: None,
        col: None,
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The editor command line: the GUI setting (`[gui] editor`), else
/// `$VISUAL`, else `$EDITOR`. Blank ones do not count.
pub fn pick_editor(configured: &str, visual: Option<&str>, editor: Option<&str>) -> Option<String> {
    [Some(configured), visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Editors that run in a terminal: the GUI starts them with no terminal, so
/// files go to the default text editor instead.
const TERMINAL_EDITORS: [&str; 17] = [
    "vi", "vim", "nvim", "view", "nano", "pico", "emacs", "hx", "helix", "micro", "kak", "ne",
    "joe", "mg", "ed", "ex", "vis",
];

/// The command that opens `path` (at `line` and `col` when known) in
/// `editor`, a command line such as `code -r`. Editors whose way of taking
/// a line is known get it; others get the file alone. Without an editor,
/// or with one that runs in a terminal, macOS's default text editor.
pub fn editor_command(
    editor: Option<&str>,
    path: &Path,
    line: Option<u32>,
    col: Option<u32>,
) -> Vec<String> {
    let file = path.to_string_lossy().into_owned();
    let fallback = || vec!["open".to_owned(), "-t".to_owned(), file.clone()];
    let Some(mut argv) = editor
        .and_then(|e| shell_words::split(e).ok())
        .filter(|a| !a.is_empty())
    else {
        return fallback();
    };
    let name = Path::new(&argv[0])
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    if TERMINAL_EDITORS.contains(&name.as_str()) {
        return fallback();
    }
    let at = |sep: &str| match (line, col) {
        (Some(l), Some(c)) => format!("{l}{sep}{c}"),
        (Some(l), None) => l.to_string(),
        _ => String::new(),
    };
    let Some(l) = line else {
        argv.push(file);
        return argv;
    };
    match name.as_str() {
        "code" | "code-insiders" | "codium" | "vscodium" | "cursor" | "windsurf" => {
            argv.extend(["-g".to_owned(), format!("{file}:{}", at(":"))]);
        }
        "zed" | "zeditor" | "subl" | "sublime_text" => argv.push(format!("{file}:{}", at(":"))),
        "mate" => argv.extend(["-l".to_owned(), at(":"), file]),
        "idea" | "pycharm" | "clion" | "rustrover" | "goland" | "webstorm" | "phpstorm"
        | "rubymine" => {
            argv.extend(["--line".to_owned(), l.to_string(), file]);
        }
        "emacsclient" => argv.extend([format!("+{}", at(":")), file]),
        "bbedit" => argv.extend([format!("+{l}"), file]),
        _ => argv.push(file),
    }
    argv
}
