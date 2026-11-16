//! Save Output, Open Output in Editor and Copy Last Output (ADR 0086):
//! file names, where files go, and what the toasts say. The text itself is
//! [`fleet_client::history`]'s, as `fleet history --plain` prints it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fleet_proto::AgentInfo;

use super::sidebar::{display_name, tilde};

/// The folder under the temp dir that Open Output in Editor writes to.
pub const TEMP_DIR: &str = "fleet-output";

/// `YYYY-MM-DD` of `unix_secs`, `utc_offset_secs` east of UTC.
pub fn date_text(unix_secs: i64, utc_offset_secs: i64) -> String {
    let days = (unix_secs + utc_offset_secs).div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The proleptic Gregorian date of a day count from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The local time zone's offset from UTC at `unix_secs`, in seconds.
pub fn local_utc_offset(unix_secs: i64) -> i64 {
    let t: libc::time_t = unix_secs;
    // SAFETY: `tm` is plain old data; all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; `localtime_r` writes only
    // into `tm` and keeps neither.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok { tm.tm_gmtoff } else { 0 }
}

/// Today's local date as [`date_text`] writes it.
pub fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    date_text(now, local_utc_offset(now))
}

/// `name` made safe as a file name: path separators, colons (shown as
/// `/` in Finder) and control characters become `-`, and a leading dot
/// is dropped so the file is not hidden. Empty becomes "agent".
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c == '/' || c == ':' || c == '\\' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim_start_matches('.');
    if s.is_empty() {
        "agent".to_owned()
    } else {
        s.to_owned()
    }
}

/// The suggested file name: `<agent-name>-<date>.txt` (its name, else its
/// program).
pub fn file_name(info: &AgentInfo, date: &str) -> String {
    format!("{}-{date}.txt", safe_name(&display_name(info)))
}

/// `dir/name`, or `dir/<stem>-2.txt`, `-3`… if `exists` says it is taken
/// (saving several agents into one folder never overwrites one with
/// another).
pub fn unique_path(dir: &Path, name: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !exists(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    (2..)
        .map(|n| {
            dir.join(match ext {
                Some(ext) => format!("{stem}-{n}.{ext}"),
                None => format!("{stem}-{n}"),
            })
        })
        .find(|p| !exists(p))
        .expect("an unused name")
}

/// Where the save panel opens: `~/Downloads` if it exists, else home.
pub fn default_save_dir(home: &Path) -> PathBuf {
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home.to_owned()
    }
}

/// Writes `text` to `path`.
pub fn write_output(path: &Path, text: &str) -> io::Result<()> {
    fs::write(path, text)
}

/// Writes `text` to `<temp>/fleet-output/<name>` (replacing an older copy)
/// and returns that path, for Open Output in Editor.
pub fn write_temp(temp: &Path, name: &str, text: &str) -> io::Result<PathBuf> {
    let dir = temp.join(TEMP_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    fs::write(&path, text)?;
    Ok(path)
}

/// The notice after a save.
pub fn saved_text(path: &Path, home: &Path) -> String {
    format!("Saved output to {}", tilde(&path.to_string_lossy(), home))
}

/// The notice after saving several agents' output into `dir`.
pub fn saved_many_text(count: usize, dir: &Path, home: &Path) -> String {
    let dir = tilde(&dir.to_string_lossy(), home);
    match count {
        1 => format!("Saved 1 agent's output to {dir}"),
        n => format!("Saved {n} agents' output to {dir}"),
    }
}

/// The error toast when reading or writing an agent's output failed.
pub fn failed_text(verb: &str, name: &str, error: &str) -> String {
    format!("Cannot {verb} the output of {name}: {error}")
}

/// The error after Save Output First when some agents' output could not
/// be saved (those agents stay); `None` when all were saved.
pub fn not_removed_text(errors: &[String]) -> Option<String> {
    match errors {
        [] => None,
        [one] => Some(format!("{one}. It was not removed.")),
        [first, rest @ ..] => Some(format!(
            "{first}. It and {} more were not removed.",
            rest.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn info(program: &str, name: Option<&str>) -> AgentInfo {
        let mut info = AgentInfo {
            id: 7,
            program: program.into(),
            ..Default::default()
        };
        if let Some(n) = name {
            info.labels.insert("name".into(), n.into());
        }
        info
    }

    #[test]
    fn dates_are_civil_and_follow_the_offset() {
        assert_eq!(date_text(0, 0), "1970-01-01");
        // 2026-11-16T23:30:00Z
        let t = 1_794_871_800;
        assert_eq!(date_text(t, 0), "2026-11-16");
        assert_eq!(date_text(t, 3600), "2026-11-17");
        assert_eq!(date_text(t, -24 * 3600), "2026-11-15");
        // A leap day, and before the epoch.
        assert_eq!(date_text(951_782_400, 0), "2000-02-29");
        assert_eq!(date_text(-1, 0), "1969-12-31");
    }

    #[test]
    fn the_file_is_named_after_the_agent_and_day() {
        assert_eq!(
            file_name(&info("/usr/bin/claude", None), "2026-11-16"),
            "claude-2026-11-16.txt"
        );
        assert_eq!(
            file_name(&info("sh", Some("sigterm-otter")), "2026-11-16"),
            "sigterm-otter-2026-11-16.txt"
        );
        assert_eq!(
            file_name(&info("sh", Some("a/b: c")), "2026-11-16"),
            "a-b- c-2026-11-16.txt"
        );
    }

    #[test]
    fn safe_names_are_never_hidden_or_empty() {
        assert_eq!(safe_name(".hidden"), "hidden");
        assert_eq!(safe_name("  "), "agent");
        assert_eq!(safe_name("tab\there"), "tab-here");
        assert_eq!(safe_name("..."), "agent");
    }

    #[test]
    fn unique_paths_count_up_past_taken_ones() {
        let dir = Path::new("/out");
        let taken: HashSet<PathBuf> = ["/out/a.txt", "/out/a-2.txt"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let exists = |p: &Path| taken.contains(p);
        assert_eq!(unique_path(dir, "b.txt", exists), dir.join("b.txt"));
        assert_eq!(unique_path(dir, "a.txt", exists), dir.join("a-3.txt"));
        let taken: HashSet<PathBuf> = [PathBuf::from("/out/x")].into();
        assert_eq!(
            unique_path(dir, "x", |p| taken.contains(p)),
            dir.join("x-2")
        );
    }

    #[test]
    fn files_are_written_and_temp_copies_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.txt");
        write_output(&path, "hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");

        let first = write_temp(tmp.path(), "a.txt", "one").unwrap();
        let second = write_temp(tmp.path(), "a.txt", "two").unwrap();
        assert_eq!(first, second);
        assert_eq!(first, tmp.path().join(TEMP_DIR).join("a.txt"));
        assert_eq!(fs::read_to_string(&second).unwrap(), "two");
        assert!(write_output(&tmp.path().join("missing/x.txt"), "").is_err());
    }

    #[test]
    fn save_dir_prefers_downloads() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(default_save_dir(home.path()), home.path());
        fs::create_dir(home.path().join("Downloads")).unwrap();
        assert_eq!(default_save_dir(home.path()), home.path().join("Downloads"));
    }

    #[test]
    fn notices_read_plainly() {
        let home = Path::new("/Users/me");
        assert_eq!(
            saved_text(Path::new("/Users/me/Downloads/a.txt"), home),
            "Saved output to ~/Downloads/a.txt"
        );
        assert_eq!(
            saved_many_text(1, Path::new("/tmp/x"), home),
            "Saved 1 agent's output to /tmp/x"
        );
        assert_eq!(
            saved_many_text(3, Path::new("/Users/me"), home),
            "Saved 3 agents' output to ~"
        );
        assert_eq!(not_removed_text(&[]), None);
        let errors = ["Cannot save a".to_owned(), "Cannot save b".to_owned()];
        assert_eq!(
            not_removed_text(&errors[..1]).as_deref(),
            Some("Cannot save a. It was not removed.")
        );
        assert_eq!(
            not_removed_text(&errors).as_deref(),
            Some("Cannot save a. It and 1 more were not removed.")
        );
        assert_eq!(
            failed_text("save", "claude", "agent 7 is not live"),
            "Cannot save the output of claude: agent 7 is not live"
        );
    }
}
