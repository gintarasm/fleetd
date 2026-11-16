//! Files dropped from Finder and images pasted into an agent (ADR 0081):
//! both become paths typed into the agent. A drop types the files'
//! shell-quoted paths, space-separated; ⌘V with an image and no text
//! writes the image to a temp file and types its path. Neither works for
//! an agent on another machine (an SSH target), where the paths would
//! not exist: a notice says so instead.

/// Shown instead of pasting an image into an agent on another machine.
pub const IMAGE_NEEDS_LOCAL: &str = "Image paste needs a local fleetd";

/// Shown instead of typing dropped paths into an agent on another machine.
pub const DROP_NEEDS_LOCAL: &str = "Dropping files needs a local fleetd";

/// Shown when the pasted image cannot be saved.
pub const IMAGE_NOT_SAVED: &str = "Could not save the pasted image";

/// What ⌘V in a pane does with the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteKind {
    /// Paste the text (it wins over an image).
    Text,
    /// Save the image to a temp file and paste its path.
    Image,
    /// Show this notice and paste nothing.
    Notice(&'static str),
    /// Nothing to paste.
    Nothing,
}

/// What ⌘V does with a clipboard that has text (`text`) or an image
/// (`image`), for an agent on this machine (`local`) or not.
pub fn paste_kind(text: bool, image: bool, local: bool) -> PasteKind {
    match (text, image, local) {
        (true, _, _) => PasteKind::Text,
        (false, true, true) => PasteKind::Image,
        (false, true, false) => PasteKind::Notice(IMAGE_NEEDS_LOCAL),
        (false, false, _) => PasteKind::Nothing,
    }
}

/// `s` as one word for a POSIX shell: unchanged when every character is
/// safe, else in single quotes (`'` as `'\''`), or as `$'…'` with escapes
/// when it holds a control character, so the result never carries a
/// newline or an escape into the agent.
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_owned();
    }
    if s.chars().any(char::is_control) {
        let mut out = String::from("$'");
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => {
                    // C0 and DEL as a byte; the C1 range (U+0080..U+009F)
                    // as a character, which `\x` would split into bytes.
                    let n = u32::from(c);
                    if n < 0x80 {
                        out.push_str(&format!("\\x{n:02x}"));
                    } else {
                        out.push_str(&format!("\\u{n:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('\'');
        return out;
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The text a drop of `paths` types: each shell-quoted, space-separated;
/// `None` for no paths.
pub fn dropped_text<S: AsRef<str>>(paths: &[S]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let words: Vec<String> = paths.iter().map(|p| shell_quote(p.as_ref())).collect();
    Some(words.join(" "))
}

/// The file extension for a pasted image's MIME type (`image/png`), if
/// it is an image type agents read.
pub fn image_extension(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" | "image/tif" => "tiff",
        _ => return None,
    })
}

/// The temp file name for an image pasted at `unix_ms`: the `attempt`th
/// try (0 first) when an earlier name was taken.
pub fn paste_file_name(unix_ms: i64, attempt: u32, ext: &str) -> String {
    if attempt == 0 {
        format!("fleet-paste-{unix_ms}.{ext}")
    } else {
        format!("fleet-paste-{unix_ms}-{attempt}.{ext}")
    }
}

/// Writes `bytes` to a new file in `dir` named by [`paste_file_name`]
/// (never over an existing one) and returns its path. Blocking: run it
/// off the UI thread.
pub fn save_pasted_image(
    dir: &std::path::Path,
    unix_ms: i64,
    ext: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    const ATTEMPTS: u32 = 100;
    for attempt in 0..ATTEMPTS {
        let path = dir.join(paste_file_name(unix_ms, attempt, ext));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free paste file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_wins_and_an_image_needs_a_local_agent() {
        assert_eq!(paste_kind(true, true, false), PasteKind::Text);
        assert_eq!(paste_kind(true, false, true), PasteKind::Text);
        assert_eq!(paste_kind(false, true, true), PasteKind::Image);
        assert_eq!(
            paste_kind(false, true, false),
            PasteKind::Notice(IMAGE_NEEDS_LOCAL)
        );
        assert_eq!(paste_kind(false, false, true), PasteKind::Nothing);
    }

    #[test]
    fn safe_words_stay_bare() {
        assert_eq!(
            shell_quote("/Users/me/a-b_c.v1,2@x+y=z:%"),
            "/Users/me/a-b_c.v1,2@x+y=z:%"
        );
    }

    #[test]
    fn spaces_and_specials_are_single_quoted() {
        assert_eq!(shell_quote("/tmp/My File.png"), "'/tmp/My File.png'");
        assert_eq!(shell_quote("/tmp/$HOME*~;&|"), "'/tmp/$HOME*~;&|'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(shell_quote("/tmp/ünï"), "'/tmp/ünï'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn control_characters_become_escapes() {
        assert_eq!(shell_quote("/tmp/a\nb"), r"$'/tmp/a\nb'");
        assert_eq!(shell_quote("a\x1b[201~'\\"), r"$'a\x1b[201~\'\\'");
        assert!(!shell_quote("x\ty\r\u{9b}").chars().any(char::is_control));
    }

    #[test]
    fn drops_join_quoted_paths_with_spaces() {
        assert_eq!(dropped_text::<&str>(&[]), None);
        assert_eq!(
            dropped_text(&["/a/b.txt", "/a/c d.png"]).as_deref(),
            Some("/a/b.txt '/a/c d.png'")
        );
    }

    #[test]
    fn image_types_map_to_extensions() {
        assert_eq!(image_extension("image/png"), Some("png"));
        assert_eq!(image_extension("image/jpeg"), Some("jpg"));
        assert_eq!(image_extension("image/tiff"), Some("tiff"));
        assert_eq!(image_extension("text/plain"), None);
    }

    #[test]
    fn paste_file_names_carry_the_time_and_avoid_taken_ones() {
        assert_eq!(paste_file_name(1700, 0, "png"), "fleet-paste-1700.png");
        assert_eq!(paste_file_name(1700, 2, "png"), "fleet-paste-1700-2.png");

        let dir = tempfile::tempdir().unwrap();
        let a = save_pasted_image(dir.path(), 5, "png", b"one").unwrap();
        let b = save_pasted_image(dir.path(), 5, "png", b"two").unwrap();
        assert_eq!(a, dir.path().join("fleet-paste-5.png"));
        assert_eq!(b, dir.path().join("fleet-paste-5-1.png"));
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
    }
}
