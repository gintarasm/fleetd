//! Shortening text to a width: by keeping its end, for paths (the last
//! directories say most about where an agent runs), or its start, for an
//! agent's last output line.

/// Where the kept tail of `text` starts when it is drawn as `…` + tail in
/// `max_w`, or `None` if all of it fits. `width_from(i)` is the width of
/// `text[i..]`; `ellipsis_w` the width of `…`. The tail starts at a `/`
/// when one is in it (`…/w/fleetd`, not `…isual/w/fleetd`).
pub fn tail_start(
    text: &str,
    max_w: f32,
    ellipsis_w: f32,
    width_from: impl Fn(usize) -> f32,
) -> Option<usize> {
    if width_from(0) <= max_w {
        return None;
    }
    let room = max_w - ellipsis_w;
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| width_from(i) <= room)
        .unwrap_or(text.len());
    let tail = &text[start..];
    // Snap to a slash with a name after it.
    let snapped = tail
        .find('/')
        .filter(|&j| j + 1 < tail.len())
        .map_or(start, |j| start + j);
    Some(snapped)
}

/// Where the kept head of `text` ends when it is drawn as head + `…` in
/// `max_w`, or `None` if all of it fits. `width_to(i)` is the width of
/// `text[..i]`; `ellipsis_w` the width of `…`. Trailing spaces of the head
/// are dropped (`compiling…`, not `compiling …`).
pub fn head_end(
    text: &str,
    max_w: f32,
    ellipsis_w: f32,
    width_to: impl Fn(usize) -> f32,
) -> Option<usize> {
    if width_to(text.len()) <= max_w {
        return None;
    }
    let room = max_w - ellipsis_w;
    let end = text
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&i| width_to(i) <= room)
        .last()
        .unwrap_or(0);
    Some(text[..end].trim_end().len())
}

#[cfg(test)]
mod tests {
    use super::{head_end, tail_start};

    /// One unit per char, keeping the start.
    fn head(text: &str, max_w: f32) -> String {
        let width_to = |i: usize| text[..i].chars().count() as f32;
        match head_end(text, max_w, 1.0, width_to) {
            None => text.to_owned(),
            Some(i) => format!("{}…", &text[..i]),
        }
    }

    #[test]
    fn head_keeps_the_start() {
        assert_eq!(head("compiling fleetd", 16.0), "compiling fleetd");
        assert_eq!(head("compiling fleetd", 40.0), "compiling fleetd");
        assert_eq!(head("", 0.0), "");
        assert_eq!(head("compiling fleetd", 8.0), "compili…");
        // No space before the ellipsis.
        assert_eq!(head("compiling fleetd", 11.0), "compiling…");
        assert_eq!(head("abcdef", 1.0), "…");
        assert_eq!(head("abcdef", 0.0), "…");
        assert_eq!(head("ąčęėįšųū", 4.0), "ąčę…");
        let t = "Running 12 tests in fleet-gui core";
        for w in 1..t.len() {
            let s = head(t, w as f32);
            assert!(s.chars().count() <= w, "{s:?} in {w}");
            assert!(s.ends_with('…') || s == t, "{s:?}");
        }
    }

    /// One unit per char.
    fn shown(text: &str, max_w: f32) -> String {
        let width_from = |i: usize| text[i..].chars().count() as f32;
        match tail_start(text, max_w, 1.0, width_from) {
            None => text.to_owned(),
            Some(i) => format!("…{}", &text[i..]),
        }
    }

    #[test]
    fn fitting_text_is_kept_whole() {
        assert_eq!(shown("~/dev/fleetd", 12.0), "~/dev/fleetd");
        assert_eq!(shown("~/dev/fleetd", 40.0), "~/dev/fleetd");
        assert_eq!(shown("", 0.0), "");
    }

    #[test]
    fn long_path_keeps_its_last_directories() {
        let p = "/private/tmp/claude-501/-Users-gintarasm/visual/w/fleetd";
        // 12 units: "…" + 11, cut back to the slash.
        assert_eq!(shown(p, 12.0), "…/w/fleetd");
        assert_eq!(shown(p, 18.0), "…/visual/w/fleetd");
        // Two directories that share a long prefix stay apart.
        let q = "/private/tmp/claude-501/-Users-gintarasm/visual/s/fleetd";
        assert_ne!(shown(p, 12.0), shown(q, 12.0));
        for w in 2..p.len() {
            let s = shown(p, w as f32);
            assert!(s.chars().count() <= w, "{s:?} in {w}");
        }
    }

    #[test]
    fn a_long_last_name_is_cut_inside_it() {
        assert_eq!(shown("/a/abcdefghij", 6.0), "…fghij");
        assert_eq!(shown("abcdefghij", 4.0), "…hij");
        // Room for the ellipsis only.
        assert_eq!(shown("abcdef", 1.0), "…");
        assert_eq!(shown("abcdef", 0.0), "…");
        // Multi-byte chars are cut on char boundaries.
        assert_eq!(shown("ąčęėįšųū", 4.0), "…šųū");
    }
}
