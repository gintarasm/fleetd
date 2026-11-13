//! Logging (ADR 0027): `tracing` events, formatted one line each, go to
//! `<state-dir>/logs/<name>` through a bounded queue and a dedicated writer
//! thread, so emitting an event never blocks on the disk. Files are rotated
//! by size; the daemon also streams new lines to `ReadLogs` followers.
//!
//! The daemon writes `fleetd.log`, each `fleetd --stdio` bridge
//! `fleetd-stdio.log`. Several processes may append to one file (bridges, or
//! a daemon that lost the instance lock), so rotation happens under an
//! exclusive lock on `<name>.lock` and only if the path still names the file
//! this process has open.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};

/// Directory under the state dir holding the log files.
pub const LOG_DIR: &str = "logs";
/// The daemon's log file.
pub const DAEMON_LOG: &str = "fleetd.log";
/// The `--stdio` bridges' log file.
pub const STDIO_LOG: &str = "fleetd-stdio.log";
/// Environment variable holding an `EnvFilter` directive (default `info`).
pub const LOG_FILTER_ENV: &str = "FLEETD_LOG";
/// A file is rotated before it would grow past this.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated files kept: `<name>.1` (newer) and `<name>.2`.
pub const KEEP_ROTATED: usize = 2;
/// Lines queued for the writer thread; more are dropped and counted.
pub const QUEUE_LINES: usize = 4096;
/// Lines queued for one `ReadLogs` follower; more are dropped and counted.
pub const FOLLOW_QUEUE_LINES: usize = 1024;
/// Most lines one `ReadLogs` returns.
pub const TAIL_MAX_LINES: u32 = 10_000;

/// Lines the writer handles per lock of the shared state.
const BATCH: usize = 256;
/// Stack of the writer thread: it formats nothing, only writes and copies.
const WRITER_STACK: usize = 128 * 1024;
/// How long the guard waits for queued lines to be written at exit.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const READ_CHUNK: usize = 64 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One log file, appended to and rotated by size.
struct LogFile {
    dir: PathBuf,
    name: String,
    max_bytes: u64,
    file: File,
    /// Bytes in the file as far as this process knows (others may append).
    size: u64,
}

fn open_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // An existing file (or the umask) may have other bits.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn rotated(dir: &Path, name: &str, n: usize) -> PathBuf {
    dir.join(format!("{name}.{n}"))
}

impl LogFile {
    /// Opens (creating) `dir/name` for appending; `dir` is created and
    /// forced to mode 0700, the file to 0600.
    fn open(dir: &Path, name: &str, max_bytes: u64) -> io::Result<LogFile> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let file = open_append(&dir.join(name))?;
        let size = file.metadata()?.len();
        Ok(LogFile {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            max_bytes,
            file,
            size,
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    fn write(&mut self, line: &[u8]) {
        let len = line.len() as u64;
        if self.size > 0 && self.size + len > self.max_bytes {
            // On failure keep appending to the file we have.
            let _ = self.rotate();
        }
        // A write error (disk full) loses the line; there is nowhere to
        // report it.
        if self.file.write_all(line).is_ok() {
            self.size += len;
        }
    }

    /// `name.1` → `name.2`, …, `name` → `name.1`, then a new `name`; under
    /// an exclusive lock on `name.lock`. If another process already rotated
    /// the file this one has open, just reopens the path.
    fn rotate(&mut self) -> io::Result<()> {
        let lock_file = open_append(&self.dir.join(format!("{}.lock", self.name)))?;
        lock_file.lock()?;
        let path = self.path();
        let ours = self.file.metadata()?.ino();
        let current = fs::metadata(&path).ok();
        if current.as_ref().is_some_and(|m| m.ino() == ours) {
            for n in (1..KEEP_ROTATED).rev() {
                match fs::rename(
                    rotated(&self.dir, &self.name, n),
                    rotated(&self.dir, &self.name, n + 1),
                ) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            fs::rename(&path, rotated(&self.dir, &self.name, 1))?;
        }
        self.file = open_append(&path)?;
        self.size = self.file.metadata()?.len();
        Ok(())
        // `lock_file` closes, releasing the lock.
    }
}

/// A `ReadLogs` follower: lines go into its bounded queue, never waiting.
struct Follower {
    tx: mpsc::Sender<String>,
    /// Lines dropped since the last one delivered.
    dropped: u64,
}

impl Follower {
    /// Queues `line` (after a drop marker, if lines were dropped); `false`
    /// once the follower is gone.
    fn offer(&mut self, line: &str) -> bool {
        if self.dropped > 0 {
            let marker = format!("… {} lines dropped", self.dropped);
            match self.tx.try_send(marker) {
                Ok(()) => self.dropped = 0,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.dropped += 1;
                    return true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        match self.tx.try_send(line.to_string()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped += 1;
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// What the writer thread and `ReadLogs` share.
struct Output {
    file: LogFile,
    followers: Vec<Follower>,
}

impl Output {
    fn emit(&mut self, line: &[u8]) {
        self.file.write(line);
        if self.followers.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\n');
        self.followers.retain_mut(|f| f.offer(text));
    }
}

/// The line noting `n` lines the queue dropped, formatted like an event.
fn dropped_note(n: u64) -> String {
    let mut line = String::new();
    let _ = SystemTime.format_time(&mut Writer::new(&mut line));
    line.push_str(&format!(
        "  WARN fleetd::logging: dropped {n} lines (log queue full)\n"
    ));
    line
}

enum Msg {
    Line(Vec<u8>),
    /// This many lines were dropped here (the queue was full).
    Dropped(u64),
    /// Write everything queued before it, then acknowledge.
    Flush(std::sync::mpsc::Sender<()>),
}

/// The subscriber's writer: hands each formatted event to the writer
/// thread's queue, dropping it (counted) when the queue is full.
struct QueueWriter {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

/// `line` (one formatted event) with every control character but its
/// final newline and tabs escaped (`\n`, `\r`, `\xNN`), so no message or
/// field, whatever its source, can start a line of its own.
fn escape_controls(line: &[u8]) -> Vec<u8> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let is_control = |b: u8| (b < 0x20 && b != b'\t') || b == 0x7f;
    let mut out = Vec::with_capacity(line.len() + 1);
    if !body.iter().any(|&b| is_control(b)) {
        out.extend_from_slice(body);
    } else {
        for &b in body {
            match b {
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b if is_control(b) => out.extend_from_slice(format!("\\x{b:02x}").as_bytes()),
                b => out.push(b),
            }
        }
    }
    out.push(b'\n');
    out
}

impl QueueWriter {
    /// Queues `line`, preceded by a note of the lines dropped before it.
    fn push(&self, line: &[u8]) {
        if self.dropped.load(Ordering::Relaxed) > 0 {
            let n = self.dropped.swap(0, Ordering::Relaxed);
            if n > 0
                && let Err(TrySendError::Full(_)) = self.tx.try_send(Msg::Dropped(n))
            {
                self.dropped.fetch_add(n + 1, Ordering::Relaxed);
                return;
            }
        }
        match self.tx.try_send(Msg::Line(escape_controls(line))) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Write for &QueueWriter {
    /// One call per event: the formatter writes each event whole.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for QueueWriter {
    type Writer = &'a QueueWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

/// Writes queued lines and notes of dropped ones.
fn run_writer(rx: Receiver<Msg>, out: Arc<Mutex<Output>>) {
    while let Ok(first) = rx.recv() {
        let mut out = lock(&out);
        let mut next = Some(first);
        let mut handled = 0;
        while let Some(msg) = next.take() {
            match msg {
                Msg::Line(line) => out.emit(&line),
                Msg::Dropped(n) => out.emit(dropped_note(n).as_bytes()),
                Msg::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
            handled += 1;
            if handled < BATCH {
                next = rx.try_recv().ok();
            }
        }
    }
}

/// The daemon's log, for `ReadLogs`.
pub struct Logs {
    out: Arc<Mutex<Output>>,
}

impl Logs {
    /// The last `tail` lines (at most [`TAIL_MAX_LINES`]) of the rotated
    /// files and the current one, oldest first; with `follow`, also a queue
    /// receiving every line written after them (none missed, none twice).
    pub fn read(&self, tail: u32, follow: bool) -> (Vec<String>, Option<mpsc::Receiver<String>>) {
        let mut out = lock(&self.out);
        let f = &out.file;
        let lines = tail_lines(&f.dir, &f.name, tail.min(TAIL_MAX_LINES) as usize);
        let rx = follow.then(|| {
            let (tx, rx) = mpsc::channel(FOLLOW_QUEUE_LINES);
            out.followers.push(Follower { tx, dropped: 0 });
            rx
        });
        (lines, rx)
    }
}

static LOGS: OnceLock<Logs> = OnceLock::new();

/// The log of this process, once [`init`] succeeded.
pub fn logs() -> Option<&'static Logs> {
    LOGS.get()
}

/// Flushes queued lines (bounded by a timeout) when dropped.
pub struct LogGuard {
    tx: SyncSender<Msg>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let (ack, done) = std::sync::mpsc::channel();
        let deadline = std::time::Instant::now() + FLUSH_TIMEOUT;
        let mut msg = Msg::Flush(ack);
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Full(m)) if std::time::Instant::now() < deadline => {
                    msg = m;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return,
            }
        }
        let _ = done.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    }
}

/// The filter from `FLEETD_LOG`; `info` when unset. An invalid directive
/// also means `info`, and is reported in the log.
fn filter() -> (EnvFilter, Option<String>) {
    match std::env::var(LOG_FILTER_ENV) {
        Ok(v) if !v.trim().is_empty() => match EnvFilter::try_new(&v) {
            Ok(f) => (f, None),
            Err(e) => (EnvFilter::new("info"), Some(format!("{v:?}: {e}"))),
        },
        _ => (EnvFilter::new("info"), None),
    }
}

/// Logs this process's events to `<state_dir>/logs/<file_name>` from now on
/// (never to stdout or stderr). Keep the guard until exit. Fails if the
/// file cannot be opened or a global subscriber is already set.
pub fn init(state_dir: &Path, file_name: &str) -> io::Result<LogGuard> {
    let file = LogFile::open(&state_dir.join(LOG_DIR), file_name, MAX_FILE_BYTES)?;
    let out = Arc::new(Mutex::new(Output {
        file,
        followers: Vec::new(),
    }));
    let (tx, rx) = sync_channel(QUEUE_LINES);
    let dropped = Arc::new(AtomicU64::new(0));
    {
        let out = Arc::clone(&out);
        std::thread::Builder::new()
            .name("log-writer".into())
            .stack_size(WRITER_STACK)
            .spawn(move || run_writer(rx, out))?;
    }
    let (filter, bad_filter) = filter();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(QueueWriter {
            tx: tx.clone(),
            dropped,
        })
        .finish();
    tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)?;
    let _ = LOGS.set(Logs { out });
    if let Some(bad) = bad_filter {
        tracing::warn!("invalid {LOG_FILTER_ENV} {bad}; using info");
    }
    Ok(LogGuard { tx })
}

/// The last `want` lines of `dir/name.2`, `.1` and `dir/name`, oldest first.
fn tail_lines(dir: &Path, name: &str, want: usize) -> Vec<String> {
    let mut newest_first = Vec::new();
    let files =
        std::iter::once(dir.join(name)).chain((1..=KEEP_ROTATED).map(|n| rotated(dir, name, n)));
    for path in files {
        if newest_first.len() >= want {
            break;
        }
        // A missing or unreadable file contributes nothing.
        let _ = last_lines(&path, want, &mut newest_first);
    }
    newest_first.reverse();
    newest_first
}

/// Pushes `path`'s lines, newest first, onto `out` until it holds `want`;
/// reads backwards in chunks, so a short tail of a big file is cheap.
fn last_lines(path: &Path, want: usize, out: &mut Vec<String>) -> io::Result<()> {
    let file = File::open(path)?;
    let mut pos = file.metadata()?.len();
    // The bytes after the last newline found so far (a line's start not
    // yet read).
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    while pos > 0 && out.len() < want {
        let step = READ_CHUNK.min(pos as usize);
        pos -= step as u64;
        file.read_exact_at(&mut buf[..step], pos)?;
        let mut chunk = buf[..step].to_vec();
        chunk.extend_from_slice(&carry);
        let mut end = chunk.len();
        while let Some(i) = chunk[..end].iter().rposition(|&b| b == b'\n') {
            if i + 1 < end {
                out.push(String::from_utf8_lossy(&chunk[i + 1..end]).into_owned());
                if out.len() >= want {
                    return Ok(());
                }
            }
            end = i;
        }
        chunk.truncate(end);
        carry = chunk;
    }
    if pos == 0 && !carry.is_empty() && out.len() < want {
        out.push(String::from_utf8_lossy(&carry).into_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total_size(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| !e.file_name().to_string_lossy().ends_with(".lock"))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    fn log_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(".lock"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(escape_controls(b"plain\tline\n"), b"plain\tline\n");
        assert_eq!(
            escape_controls(b"a\nT ERROR fake\r\x1b[31m\x7f\n"),
            b"a\\nT ERROR fake\\r\\x1b[31m\\x7f\n"
        );
        // A missing final newline is added.
        assert_eq!(escape_controls(b"x"), b"x\n");
    }

    #[test]
    fn rotation_caps_total_size() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let mut f = LogFile::open(&dir, "t.log", 1000).unwrap();
        for i in 0..1000 {
            f.write(format!("line {i:04} ......................................\n").as_bytes());
        }
        assert_eq!(log_files(&dir), ["t.log", "t.log.1", "t.log.2"]);
        assert!(total_size(&dir) <= 3 * 1000, "{}", total_size(&dir));
        for name in log_files(&dir) {
            let len = fs::metadata(dir.join(&name)).unwrap().len();
            assert!(len <= 1000, "{name}: {len}");
            let mode = fs::metadata(dir.join(&name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }
        // The newest lines survive, in order, across the files.
        let tail = tail_lines(&dir, "t.log", 10_000);
        assert!(tail.last().unwrap().starts_with("line 0999"));
        let nums: Vec<u32> = tail.iter().map(|l| l[5..9].parse().unwrap()).collect();
        assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "{nums:?}");
    }

    #[test]
    fn two_writers_rotating_one_file_lose_no_line_shape() {
        // Two processes (here: two handles) appending to and rotating the
        // same file: every line stays whole and at most three files exist.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        let mut a = LogFile::open(&dir, "s.log", 2000).unwrap();
        let mut b = LogFile::open(&dir, "s.log", 2000).unwrap();
        for i in 0..600 {
            a.write(format!("a {i:04} ------------------------\n").as_bytes());
            b.write(format!("b {i:04} ------------------------\n").as_bytes());
        }
        assert_eq!(log_files(&dir), ["s.log", "s.log.1", "s.log.2"]);
        for name in log_files(&dir) {
            let text = fs::read_to_string(dir.join(&name)).unwrap();
            for line in text.lines() {
                assert_eq!(line.len(), 31, "{name}: torn line {line:?}");
            }
        }
        // Both writers' newest lines are in the newest files.
        let tail = tail_lines(&dir, "s.log", 10);
        assert!(tail.iter().any(|l| l.starts_with("a 0599")), "{tail:?}");
        assert!(tail.iter().any(|l| l.starts_with("b 0599")), "{tail:?}");
    }

    #[test]
    fn tail_counts_lines_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("x.log.2"), "a\nb\n").unwrap();
        fs::write(dir.join("x.log.1"), "c\nd\n").unwrap();
        // The current file's last line may still lack its newline.
        fs::write(dir.join("x.log"), "e\nf").unwrap();
        assert_eq!(tail_lines(dir, "x.log", 3), ["d", "e", "f"]);
        assert_eq!(
            tail_lines(dir, "x.log", 100),
            ["a", "b", "c", "d", "e", "f"]
        );
        assert!(tail_lines(dir, "x.log", 0).is_empty());
        assert!(tail_lines(dir, "missing.log", 5).is_empty());
        // Lines longer than a read chunk.
        let long = "z".repeat(READ_CHUNK + 10);
        fs::write(dir.join("y.log"), format!("first\n{long}\nlast\n")).unwrap();
        assert_eq!(
            tail_lines(dir, "y.log", 3),
            ["first", long.as_str(), "last"]
        );
    }

    fn output(dir: &Path) -> Arc<Mutex<Output>> {
        Arc::new(Mutex::new(Output {
            file: LogFile::open(dir, "q.log", MAX_FILE_BYTES).unwrap(),
            followers: Vec::new(),
        }))
    }

    #[test]
    fn full_queue_drops_and_the_next_line_notes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let out = output(tmp.path());
        let (tx, rx) = sync_channel(4);
        let w = QueueWriter {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        // The writer thread is not running yet: 4 fit, 6 are dropped.
        for i in 0..10 {
            (&w).write_all(format!("l{i}\n").as_bytes()).unwrap();
        }
        assert_eq!(w.dropped.load(Ordering::Relaxed), 6);
        let writer = {
            let out = Arc::clone(&out);
            std::thread::spawn(move || run_writer(rx, out))
        };
        // Wait until the writer drained the queue, then queue one more line:
        // the note marks the gap, before it.
        let (ack, done) = std::sync::mpsc::channel();
        w.tx.send(Msg::Flush(ack)).unwrap();
        done.recv().unwrap();
        let (ack, done) = std::sync::mpsc::channel();
        (&w).write_all(b"after\n").unwrap();
        w.tx.send(Msg::Flush(ack)).unwrap();
        done.recv().unwrap();
        drop(w);
        writer.join().unwrap();
        let text = fs::read_to_string(tmp.path().join("q.log")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[..4], ["l0", "l1", "l2", "l3"]);
        assert!(
            lines[4].contains("WARN fleetd::logging: dropped 6 lines"),
            "{text}"
        );
        assert_eq!(lines[5], "after");
    }

    #[test]
    fn follower_gets_a_drop_marker_and_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = Logs {
            out: output(tmp.path()),
        };
        let (_, rx) = logs.read(0, true);
        let mut rx = rx.unwrap();
        for i in 0..FOLLOW_QUEUE_LINES + 5 {
            lock(&logs.out).emit(format!("x{i}\n").as_bytes());
        }
        let mut got = Vec::new();
        while let Ok(l) = rx.try_recv() {
            got.push(l);
        }
        assert_eq!(got.len(), FOLLOW_QUEUE_LINES);
        lock(&logs.out).emit(b"next\n");
        assert_eq!(rx.try_recv().unwrap(), "… 5 lines dropped");
        assert_eq!(rx.try_recv().unwrap(), "next");
        // A follower that went away is forgotten.
        drop(rx);
        lock(&logs.out).emit(b"gone\n");
        assert!(lock(&logs.out).followers.is_empty());
    }

    #[test]
    fn read_then_follow_misses_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = Logs {
            out: output(tmp.path()),
        };
        lock(&logs.out).emit(b"one\n");
        let (tail, rx) = logs.read(10, true);
        lock(&logs.out).emit(b"two\n");
        assert_eq!(tail, ["one"]);
        assert_eq!(rx.unwrap().try_recv().unwrap(), "two");
    }
}
