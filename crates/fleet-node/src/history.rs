//! Per-agent history store: an in-memory tail of recent lines plus on-disk
//! segments holding everything that scrolled off the terminal.
//!
//! Layout under the agent's history directory (0700):
//! - `<first_line:020>.seg`: records `[u32 LE len][bytes]`, one per line, rolled
//!   over at `SEGMENT_BYTES`;
//! - `meta`: `[u64 LE generation][u64 LE first_line]`, rewritten on `clear`.
//!
//! Appends accumulate in a 64 KiB buffer and reach the file when it fills, on
//! `flush()` or on drop; there is never an fsync. Lines still in the buffer are
//! served by `read_page` straight from memory. The only in-memory index is one
//! byte offset per 64 lines per segment, so a page read seeks to the nearest
//! indexed record and skips at most 63 records before the page.

use std::collections::VecDeque;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use bytes::Bytes;

/// In-memory tail cap per agent.
pub const HISTORY_TAIL_BYTES: usize = 256 * 1024;
/// On-disk cap per agent.
pub const HISTORY_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Segment rollover size.
pub const SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum payload bytes in one page.
pub const HISTORY_PAGE_MAX_BYTES: usize = 256 * 1024;
/// Maximum lines in one page.
pub const HISTORY_PAGE_MAX_LINES: u32 = 4096;

const PENDING_MAX: usize = 64 * 1024;
const INDEX_STRIDE: u64 = 64;
const READ_AHEAD: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryInfo {
    pub first_line: u64,
    pub next_line: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Line number of `lines[0]` (== the requested end when `lines` is empty).
    pub first_line: u64,
    pub lines: Vec<Bytes>,
}

#[derive(Clone, Copy)]
struct Limits {
    tail_bytes: usize,
    max_bytes: u64,
    segment_bytes: u64,
}

const DEFAULT_LIMITS: Limits = Limits {
    tail_bytes: HISTORY_TAIL_BYTES,
    max_bytes: HISTORY_MAX_BYTES,
    segment_bytes: SEGMENT_BYTES,
};

struct Segment {
    first_line: u64,
    lines: u64,
    /// Logical size including bytes still in the write buffer.
    bytes: u64,
    /// Byte offset of every `INDEX_STRIDE`th record.
    index: Vec<u64>,
    path: PathBuf,
}

pub struct History {
    dir: PathBuf,
    limits: Limits,
    generation: u64,
    first_line: u64,
    next_line: u64,
    segments: Vec<Segment>,
    /// Append handle (also used for reads) of the last segment.
    active: File,
    pending: Vec<u8>,
    /// Set after a disk write failure; further pushes only feed the tail.
    broken: bool,
    tail: VecDeque<Bytes>,
    tail_bytes: usize,
}

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn seg_path(dir: &Path, first_line: u64) -> PathBuf {
    dir.join(format!("{first_line:020}.seg"))
}

fn create_seg_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

fn write_meta(dir: &Path, generation: u64, first_line: u64) -> io::Result<()> {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&generation.to_le_bytes());
    buf[8..].copy_from_slice(&first_line.to_le_bytes());
    let tmp = dir.join("meta.tmp");
    {
        use std::io::Write;
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&buf)?;
    }
    fs::rename(&tmp, dir.join("meta"))
}

fn read_meta(dir: &Path) -> io::Result<(u64, u64)> {
    match fs::read(dir.join("meta")) {
        Ok(b) if b.len() == 16 => Ok((
            u64::from_le_bytes(b[..8].try_into().unwrap()),
            u64::from_le_bytes(b[8..].try_into().unwrap()),
        )),
        Ok(_) => Err(invalid_data("history meta malformed")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((0, 0)),
        Err(e) => Err(e),
    }
}

/// Sequential reader over a segment's logical byte space: file bytes up to
/// `flushed`, then the in-memory write buffer.
struct SegReader<'a> {
    file: &'a File,
    flushed: u64,
    pending: &'a [u8],
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
}

impl<'a> SegReader<'a> {
    fn new(file: &'a File, flushed: u64, pending: &'a [u8], pos: u64) -> Self {
        SegReader {
            file,
            flushed,
            pending,
            pos,
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    fn read_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < out.len() {
            let want = out.len() - done;
            if self.pos >= self.flushed {
                let off = (self.pos - self.flushed) as usize;
                if off + want > self.pending.len() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                out[done..].copy_from_slice(&self.pending[off..off + want]);
                self.pos += want as u64;
                return Ok(());
            }
            let buf_end = self.buf_start + self.buf.len() as u64;
            if self.pos >= self.buf_start && self.pos < buf_end {
                let off = (self.pos - self.buf_start) as usize;
                let n = want.min(self.buf.len() - off);
                out[done..done + n].copy_from_slice(&self.buf[off..off + n]);
                done += n;
                self.pos += n as u64;
            } else {
                let n = ((self.flushed - self.pos) as usize).min(READ_AHEAD.max(want));
                self.buf.resize(n, 0);
                self.file.read_exact_at(&mut self.buf, self.pos)?;
                self.buf_start = self.pos;
            }
        }
        Ok(())
    }

    fn skip_record(&mut self) -> io::Result<()> {
        let mut len = [0u8; 4];
        self.read_exact(&mut len)?;
        self.pos += u32::from_le_bytes(len) as u64;
        Ok(())
    }

    fn record(&mut self) -> io::Result<Bytes> {
        let mut len = [0u8; 4];
        self.read_exact(&mut len)?;
        let mut v = vec![0u8; u32::from_le_bytes(len) as usize];
        self.read_exact(&mut v)?;
        Ok(Bytes::from(v))
    }
}

impl History {
    /// Create a fresh history directory (mode 0700) for a new agent.
    pub fn create(dir: &Path) -> io::Result<History> {
        Self::create_with(dir, DEFAULT_LIMITS)
    }

    fn create_with(dir: &Path, limits: Limits) -> io::Result<History> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        write_meta(dir, 0, 0)?;
        let path = seg_path(dir, 0);
        let active = create_seg_file(&path)?;
        active.set_len(0)?;
        Ok(History {
            dir: dir.to_path_buf(),
            limits,
            generation: 0,
            first_line: 0,
            next_line: 0,
            segments: vec![Segment {
                first_line: 0,
                lines: 0,
                bytes: 0,
                index: Vec::new(),
                path,
            }],
            active,
            pending: Vec::new(),
            broken: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
        })
    }

    #[cfg(test)]
    fn with_limits(
        dir: &Path,
        tail_bytes: usize,
        max_bytes: u64,
        segment_bytes: u64,
    ) -> io::Result<History> {
        Self::create_with(
            dir,
            Limits {
                tail_bytes,
                max_bytes,
                segment_bytes,
            },
        )
    }

    /// Open an existing history directory, rebuilding the sparse index by
    /// scanning every segment. A torn trailing record is truncated away.
    pub fn open(dir: &Path) -> io::Result<History> {
        Self::open_with(dir, DEFAULT_LIMITS)
    }

    fn open_with(dir: &Path, limits: Limits) -> io::Result<History> {
        let (generation, meta_first) = read_meta(dir)?;
        let mut found: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("seg") {
                continue;
            }
            let first = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| invalid_data("bad segment name"))?;
            found.push((first, path));
        }
        found.sort();
        let mut segments = Vec::with_capacity(found.len());
        for (first_line, path) in found {
            segments.push(scan_segment(first_line, path)?);
        }
        if segments.is_empty() {
            let path = seg_path(dir, meta_first);
            create_seg_file(&path)?;
            segments.push(Segment {
                first_line: meta_first,
                lines: 0,
                bytes: 0,
                index: Vec::new(),
                path,
            });
        }
        for w in segments.windows(2) {
            if w[0].first_line + w[0].lines != w[1].first_line {
                return Err(invalid_data("history segments not contiguous"));
            }
        }
        let last = segments.last().unwrap();
        let next_line = last.first_line + last.lines;
        let first_line = meta_first.max(segments[0].first_line).min(next_line);
        let active = create_seg_file(&last.path)?;
        let mut h = History {
            dir: dir.to_path_buf(),
            limits,
            generation,
            first_line,
            next_line,
            segments,
            active,
            pending: Vec::new(),
            broken: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
        };
        h.rebuild_tail()?;
        Ok(h)
    }

    fn rebuild_tail(&mut self) -> io::Result<()> {
        let start = self
            .next_line
            .saturating_sub(HISTORY_PAGE_MAX_LINES as u64)
            .max(self.first_line);
        let lines = self.read_range(start, self.next_line)?;
        for l in lines {
            self.tail_bytes += l.len();
            self.tail.push_back(l);
        }
        self.trim_tail();
        Ok(())
    }

    fn trim_tail(&mut self) {
        while self.tail_bytes > self.limits.tail_bytes {
            match self.tail.pop_front() {
                Some(l) => self.tail_bytes -= l.len(),
                None => break,
            }
        }
        let tail_first = self.next_line - self.tail.len() as u64;
        if tail_first < self.first_line {
            for _ in 0..(self.first_line - tail_first) {
                if let Some(l) = self.tail.pop_front() {
                    self.tail_bytes -= l.len();
                }
            }
        }
    }

    /// Append one line (no newline). Cheap: a buffered append, with a write
    /// syscall only when the 64 KiB buffer fills.
    pub fn push(&mut self, line: &[u8]) {
        let len = line.len().min(u32::MAX as usize);
        let line = &line[..len];
        let rec = 4 + len as u64;
        if !self.broken && self.try_push_disk(line, rec).is_err() {
            self.broken = true;
            self.pending.clear();
        }
        self.next_line += 1;
        let b = Bytes::copy_from_slice(line);
        self.tail_bytes += b.len();
        self.tail.push_back(b);
        self.trim_tail();
    }

    fn try_push_disk(&mut self, line: &[u8], rec: u64) -> io::Result<()> {
        let active = self.segments.last().unwrap();
        if active.lines > 0 && active.bytes + rec > self.limits.segment_bytes {
            self.roll()?;
        }
        if self.pending.len() as u64 + rec > PENDING_MAX as u64 {
            self.flush()?;
        }
        let seg = self.segments.last_mut().unwrap();
        if seg.lines.is_multiple_of(INDEX_STRIDE) {
            seg.index.push(seg.bytes);
        }
        self.pending
            .extend_from_slice(&(line.len() as u32).to_le_bytes());
        self.pending.extend_from_slice(line);
        seg.lines += 1;
        seg.bytes += rec;
        Ok(())
    }

    fn roll(&mut self) -> io::Result<()> {
        self.flush()?;
        let path = seg_path(&self.dir, self.next_line);
        self.active = create_seg_file(&path)?;
        self.segments.push(Segment {
            first_line: self.next_line,
            lines: 0,
            bytes: 0,
            index: Vec::new(),
            path,
        });
        // Enforce the on-disk cap by dropping the oldest whole segments.
        let mut total: u64 = self.segments.iter().map(|s| s.bytes).sum();
        while total > self.limits.max_bytes && self.segments.len() > 1 {
            let old = self.segments.remove(0);
            total -= old.bytes;
            let _ = fs::remove_file(&old.path);
            self.first_line = self.segments[0].first_line;
        }
        Ok(())
    }

    /// Drop all history (CSI 3 J): bump the generation, delete segments.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.generation += 1;
        self.first_line = self.next_line;
        self.tail.clear();
        self.tail_bytes = 0;
        for s in self.segments.drain(..) {
            let _ = fs::remove_file(&s.path);
        }
        let path = seg_path(&self.dir, self.next_line);
        let ok = write_meta(&self.dir, self.generation, self.first_line).is_ok();
        match create_seg_file(&path).and_then(|f| f.set_len(0).map(|_| f)) {
            Ok(f) => {
                self.active = f;
                self.broken = !ok;
            }
            Err(_) => self.broken = true,
        }
        self.segments.push(Segment {
            first_line: self.next_line,
            lines: 0,
            bytes: 0,
            index: Vec::new(),
            path,
        });
    }

    /// The in-memory tail: the line number of its first line, and the lines.
    pub fn tail(&self) -> (u64, Vec<Bytes>) {
        (
            self.next_line - self.tail.len() as u64,
            self.tail.iter().cloned().collect(),
        )
    }

    pub fn info(&self) -> HistoryInfo {
        HistoryInfo {
            first_line: self.first_line,
            next_line: self.next_line,
            generation: self.generation,
        }
    }

    /// Write the buffer to the active segment (no fsync).
    pub fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        use std::io::Write;
        let r = (&self.active).write_all(&self.pending);
        self.pending.clear();
        r
    }

    /// The lines immediately before `before_line`, newest last, capped by
    /// `max_lines`, the page caps, and `first_line`.
    pub fn read_page(&self, generation: u64, before_line: u64, max_lines: u32) -> io::Result<Page> {
        if generation != self.generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "history cleared",
            ));
        }
        let end = before_line.min(self.next_line).max(self.first_line);
        let n = max_lines.min(HISTORY_PAGE_MAX_LINES) as u64;
        let start = end.saturating_sub(n).max(self.first_line);
        let mut lines = self.read_range(start, end)?;
        let mut bytes: usize = lines.iter().map(Bytes::len).sum();
        let mut skip = 0;
        while bytes > HISTORY_PAGE_MAX_BYTES && lines.len() - skip > 1 {
            bytes -= lines[skip].len();
            skip += 1;
        }
        lines.drain(..skip);
        Ok(Page {
            first_line: end - lines.len() as u64,
            lines,
        })
    }

    /// Read lines `[start, end)`; both must lie within `[first_line, next_line]`.
    fn read_range(&self, start: u64, end: u64) -> io::Result<Vec<Bytes>> {
        let mut out = Vec::with_capacity((end - start) as usize);
        if start >= end {
            return Ok(out);
        }
        let last = self.segments.len() - 1;
        for (i, seg) in self.segments.iter().enumerate() {
            let seg_end = seg.first_line + seg.lines;
            if seg_end <= start || seg.first_line >= end {
                continue;
            }
            let from = start.max(seg.first_line);
            let to = end.min(seg_end);
            let rel = from - seg.first_line;
            let off = *seg
                .index
                .get((rel / INDEX_STRIDE) as usize)
                .ok_or_else(|| invalid_data("history index out of range"))?;
            let opened;
            let (file, flushed, pending): (&File, u64, &[u8]) = if i == last {
                (
                    &self.active,
                    seg.bytes - self.pending.len() as u64,
                    &self.pending,
                )
            } else {
                opened = File::open(&seg.path)?;
                (&opened, seg.bytes, &[])
            };
            let mut r = SegReader::new(file, flushed, pending, off);
            for _ in 0..(rel % INDEX_STRIDE) {
                r.skip_record()?;
            }
            for _ in from..to {
                out.push(r.record()?);
            }
        }
        Ok(out)
    }
}

impl Drop for History {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Scan a segment, building its sparse index and truncating a torn tail.
fn scan_segment(first_line: u64, path: PathBuf) -> io::Result<Segment> {
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let len = file.metadata()?.len();
    let mut r = SegReader::new(&file, len, &[], 0);
    let mut index = Vec::new();
    let mut lines = 0u64;
    let mut good = 0u64;
    loop {
        if r.pos + 4 > len {
            break;
        }
        let mut hdr = [0u8; 4];
        r.read_exact(&mut hdr)?;
        let rec_len = u32::from_le_bytes(hdr) as u64;
        if r.pos + rec_len > len {
            break;
        }
        if lines.is_multiple_of(INDEX_STRIDE) {
            index.push(good);
        }
        r.pos += rec_len;
        good = r.pos;
        lines += 1;
    }
    if good < len {
        file.set_len(good)?;
    }
    Ok(Segment {
        first_line,
        lines,
        bytes: good,
        index,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn line(i: u64) -> Vec<u8> {
        format!("line {i}").into_bytes()
    }

    fn texts(p: &Page) -> Vec<String> {
        p.lines
            .iter()
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect()
    }

    fn fill(h: &mut History, n: u64) {
        for i in 0..n {
            h.push(&line(i));
        }
    }

    #[test]
    fn push_then_read_page_returns_lines_before() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History::create(&d.path().join("h")).unwrap();
        fill(&mut h, 200);
        // Unflushed lines are readable.
        let p = h.read_page(0, 100, 10).unwrap();
        assert_eq!(p.first_line, 90);
        assert_eq!(
            texts(&p),
            (90..100).map(|i| format!("line {i}")).collect::<Vec<_>>()
        );
        let p = h.read_page(0, 200, 3).unwrap();
        assert_eq!(texts(&p), ["line 197", "line 198", "line 199"]);
        // Crossing an index stride boundary, after a flush too.
        h.flush().unwrap();
        let p = h.read_page(0, 131, 5).unwrap();
        assert_eq!(p.first_line, 126);
        assert_eq!(texts(&p)[0], "line 126");
        // Never below first_line, clamped past the end.
        let p = h.read_page(0, 5, 50).unwrap();
        assert_eq!((p.first_line, p.lines.len()), (0, 5));
        let p = h.read_page(0, 9999, 1).unwrap();
        assert_eq!(texts(&p), ["line 199"]);
        assert!(h.read_page(0, 0, 10).unwrap().lines.is_empty());
    }

    #[test]
    fn page_respects_line_and_byte_caps() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History::create(&d.path().join("h")).unwrap();
        fill(&mut h, HISTORY_PAGE_MAX_LINES as u64 + 500);
        let p = h.read_page(0, u64::MAX, u32::MAX).unwrap();
        assert_eq!(p.lines.len(), HISTORY_PAGE_MAX_LINES as usize);
        assert_eq!(p.first_line, 500);

        let d2 = tempfile::tempdir().unwrap();
        let mut h = History::create(&d2.path().join("h")).unwrap();
        let big = vec![b'x'; 10 * 1024];
        for _ in 0..100 {
            h.push(&big);
        }
        let p = h.read_page(0, 100, 100).unwrap();
        let total: usize = p.lines.iter().map(Bytes::len).sum();
        assert!(total <= HISTORY_PAGE_MAX_BYTES);
        assert_eq!(p.lines.len(), 25);
        assert_eq!(p.first_line, 75);
    }

    #[test]
    fn tail_is_bounded_and_most_recent() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History::with_limits(&d.path().join("h"), 100, 1 << 20, 1 << 20).unwrap();
        fill(&mut h, 1000);
        let (first, lines) = h.tail();
        let total: usize = lines.iter().map(Bytes::len).sum();
        assert!(total <= 100 && !lines.is_empty());
        assert_eq!(first + lines.len() as u64, 1000);
        assert_eq!(lines.last().unwrap().as_ref(), b"line 999");
    }

    #[test]
    fn read_after_clear_serves_only_new_generation() {
        let d = tempfile::tempdir().unwrap();
        let mut h = History::create(&d.path().join("h")).unwrap();
        fill(&mut h, 100);
        h.clear();
        let info = h.info();
        assert_eq!(
            info,
            HistoryInfo {
                first_line: 100,
                next_line: 100,
                generation: 1
            }
        );
        let e = h.read_page(0, 100, 10).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(e.to_string(), "history cleared");
        assert!(h.tail().1.is_empty());
        for i in 0..5 {
            h.push(&line(1000 + i));
        }
        let p = h.read_page(1, 105, 50).unwrap();
        assert_eq!(p.first_line, 100);
        assert_eq!(texts(&p)[0], "line 1000");
        assert_eq!(p.lines.len(), 5);
        let p = h.read_page(1, 100, 50).unwrap();
        assert!(p.lines.is_empty());
    }

    #[test]
    fn cap_drops_oldest_segments() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::with_limits(&dir, 1024, 2000, 500).unwrap();
        fill(&mut h, 1000);
        let info = h.info();
        assert!(info.first_line > 0);
        assert_eq!(info.next_line, 1000);
        h.flush().unwrap();
        let disk: u64 = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().extension().is_some_and(|x| x == "seg"))
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(disk <= 2000 + 500, "disk {disk}");
        let p = h.read_page(0, 1000, 5000).unwrap();
        assert_eq!(p.first_line, info.first_line);
        assert_eq!(texts(&p)[0], format!("line {}", info.first_line));
        assert_eq!(p.first_line + p.lines.len() as u64, 1000);
    }

    #[test]
    fn reopen_restores_info_and_pages() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::with_limits(&dir, 1024, 1 << 20, 600).unwrap();
        fill(&mut h, 500);
        h.clear();
        for i in 0..300 {
            h.push(&line(i));
        }
        let info = h.info();
        drop(h);
        let h = History::open(&dir).unwrap();
        assert_eq!(h.info(), info);
        let p = h.read_page(1, 300 + 500, 4).unwrap();
        assert_eq!(texts(&p), ["line 296", "line 297", "line 298", "line 299"]);
        let p = h.read_page(1, 500 + 70, 3).unwrap();
        assert_eq!(texts(&p), ["line 67", "line 68", "line 69"]);
        assert!(!h.tail().1.is_empty());
        assert!(h.read_page(0, 800, 5).is_err());
    }

    #[test]
    fn files_are_private() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::create(&dir).unwrap();
        fill(&mut h, 10);
        h.flush().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("meta")), 0o600);
        assert_eq!(mode(&seg_path(&dir, 0)), 0o600);
        h.clear();
        assert_eq!(mode(&dir.join("meta")), 0o600);
        assert_eq!(mode(&seg_path(&dir, 10)), 0o600);
    }
}
