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
    pending: Vec<u8>,
    /// Records currently in `pending`.
    pending_lines: u64,
    /// Set after a disk write failure; further pushes only feed the tail.
    broken: bool,
    tail: Tail,
    /// Test-only: make the next flush write half the buffer, then fail.
    #[cfg(test)]
    fault: bool,
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

/// The in-memory tail: the most recent lines' bytes back to back in one ring
/// buffer, plus each line's length. At most `limit` bytes (the `limit` passed
/// to [`Tail::push`]); a line longer than that empties it.
#[derive(Default)]
struct Tail {
    buf: VecDeque<u8>,
    lens: VecDeque<u32>,
}

impl Tail {
    fn lines(&self) -> usize {
        self.lens.len()
    }

    fn push(&mut self, line: &[u8], limit: usize) {
        if line.len() > limit {
            self.clear();
            return;
        }
        while self.buf.len() + line.len() > limit {
            self.pop_front();
        }
        let need = self.buf.len() + line.len();
        if need > self.buf.capacity() {
            // Double up to `limit`, never beyond: the ring never holds more.
            let want = (self.buf.capacity() * 2).max(4096).min(limit).max(need);
            self.buf.reserve_exact(want - self.buf.len());
        }
        self.buf.extend(line);
        // `limit` is far below `u32::MAX`, and so is `line.len()`.
        self.lens.push_back(line.len() as u32);
    }

    fn pop_front(&mut self) {
        if let Some(n) = self.lens.pop_front() {
            self.buf.drain(..n as usize);
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.lens.clear();
    }

    /// The lines, as slices of one copy of the buffer.
    fn to_lines(&self) -> Vec<Bytes> {
        let (a, b) = self.buf.as_slices();
        let mut all = Vec::with_capacity(a.len() + b.len());
        all.extend_from_slice(a);
        all.extend_from_slice(b);
        let all = Bytes::from(all);
        let mut at = 0;
        self.lens
            .iter()
            .map(|&n| {
                let line = all.slice(at..at + n as usize);
                at += n as usize;
                line
            })
            .collect()
    }
}

/// Sequential reader over a segment's logical byte space: file bytes up to
/// `flushed`, then the in-memory write buffer.
struct SegReader<'a> {
    file: &'a File,
    flushed: u64,
    pending: &'a [u8],
    /// Logical end of the segment; records may not extend past it.
    limit: u64,
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
            limit: flushed + pending.len() as u64,
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
        let n = u32::from_le_bytes(len) as u64;
        if self.pos + n > self.limit {
            return Err(invalid_data("history record overruns segment"));
        }
        self.pos += n;
        Ok(())
    }

    fn record(&mut self) -> io::Result<Bytes> {
        let mut len = [0u8; 4];
        self.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as u64;
        if self.pos + n > self.limit {
            return Err(invalid_data("history record overruns segment"));
        }
        let mut v = vec![0u8; n as usize];
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
        create_seg_file(&path)?.set_len(0)?;
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
            pending: Vec::new(),
            pending_lines: 0,
            broken: false,
            #[cfg(test)]
            fault: false,
            tail: Tail::default(),
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
        create_seg_file(&last.path)?;
        let mut h = History {
            dir: dir.to_path_buf(),
            limits,
            generation,
            first_line,
            next_line,
            segments,
            pending: Vec::new(),
            pending_lines: 0,
            broken: false,
            #[cfg(test)]
            fault: false,
            tail: Tail::default(),
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
            self.tail.push(&l, self.limits.tail_bytes);
        }
        self.trim_tail();
        Ok(())
    }

    /// Drops tail lines that precede `first_line` (evicted from disk).
    fn trim_tail(&mut self) {
        let tail_first = self.next_line - self.tail.lines() as u64;
        for _ in tail_first..self.first_line {
            self.tail.pop_front();
        }
    }

    /// Append one line (no newline). Cheap: a buffered append, with a write
    /// syscall only when the 64 KiB buffer fills.
    pub fn push(&mut self, line: &[u8]) {
        let len = line.len().min(u32::MAX as usize);
        let line = &line[..len];
        let rec = 4 + len as u64;
        if !self.broken && self.try_push_disk(line, rec).is_err() {
            self.fail();
        }
        self.next_line += 1;
        self.tail.push(line, self.limits.tail_bytes);
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
        self.pending_lines += 1;
        Ok(())
    }

    /// Give up on the disk after a write error: roll the active segment's
    /// accounting back to what is really in the file (truncating any partial
    /// write) and stop writing. Reads past the durable end then error out.
    fn fail(&mut self) {
        self.broken = true;
        let seg = self.segments.last_mut().unwrap();
        let good = seg.bytes - self.pending.len() as u64;
        seg.bytes = good;
        seg.lines -= self.pending_lines;
        seg.index.retain(|&o| o < good);
        self.pending.clear();
        self.pending_lines = 0;
        if let Ok(f) = OpenOptions::new().write(true).open(&seg.path) {
            let _ = f.set_len(good);
        }
    }

    fn roll(&mut self) -> io::Result<()> {
        self.flush()?;
        let path = seg_path(&self.dir, self.next_line);
        create_seg_file(&path)?;
        self.segments.push(Segment {
            first_line: self.next_line,
            lines: 0,
            bytes: 0,
            index: Vec::new(),
            path,
        });
        // Enforce the on-disk cap by dropping the oldest whole segments.
        let mut total: u64 = self.segments.iter().map(|s| s.bytes).sum();
        // Leave room for the new segment to fill up, so the total stays <= cap.
        while total + self.limits.segment_bytes > self.limits.max_bytes && self.segments.len() > 1 {
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
        self.pending_lines = 0;
        self.generation += 1;
        self.first_line = self.next_line;
        self.tail.clear();
        for s in self.segments.drain(..) {
            let _ = fs::remove_file(&s.path);
        }
        let path = seg_path(&self.dir, self.next_line);
        let ok = write_meta(&self.dir, self.generation, self.first_line).is_ok();
        self.broken = !ok || create_seg_file(&path).and_then(|f| f.set_len(0)).is_err();
        self.segments.push(Segment {
            first_line: self.next_line,
            lines: 0,
            bytes: 0,
            index: Vec::new(),
            path,
        });
    }

    /// The in-memory tail: the line number of its first line (not the store's
    /// `first_line`; it maps to `AttachOk.tail_first_line`), and the lines.
    ///
    /// One copy of the tail's bytes; the lines are slices of it.
    pub fn tail(&self) -> (u64, Vec<Bytes>) {
        (
            self.next_line - self.tail.lines() as u64,
            self.tail.to_lines(),
        )
    }

    pub fn info(&self) -> HistoryInfo {
        HistoryInfo {
            first_line: self.first_line,
            next_line: self.next_line,
            generation: self.generation,
        }
    }

    /// Write the buffer to the active segment (no fsync). The segment is
    /// opened for each flush and closed after it: a history holds no fd
    /// between writes.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        use std::io::Write;
        let path = &self.segments.last().unwrap().path;
        let written = create_seg_file(path).and_then(|mut f| {
            #[cfg(test)]
            if self.fault {
                let _ = f.write_all(&self.pending[..self.pending.len() / 2]);
                return Err(io::Error::other("injected write fault"));
            }
            f.write_all(&self.pending)
        });
        match written {
            Ok(()) => {
                self.pending.clear();
                self.pending_lines = 0;
                Ok(())
            }
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }

    fn disk_end(&self) -> u64 {
        let last = self.segments.last().unwrap();
        last.first_line + last.lines
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
        if end > self.disk_end() {
            return Err(io::Error::other("history unavailable after write error"));
        }
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
        if end > self.disk_end() {
            return Err(io::Error::other("history unavailable after write error"));
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
            let (flushed, pending): (u64, &[u8]) = if i == last {
                (seg.bytes - self.pending.len() as u64, &self.pending)
            } else {
                (seg.bytes, &[])
            };
            let file = File::open(&seg.path)?;
            let mut r = SegReader::new(&file, flushed, pending, off);
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
    fn tail_ring_wraps_and_stays_within_its_limit() {
        let mut t = Tail::default();
        let mut all = Vec::new();
        for i in 0..5000u32 {
            let l = vec![b'a' + (i % 26) as u8; (i as usize * 37) % 300];
            t.push(&l, 1000);
            all.push(l);
            assert!(t.buf.len() <= 1000 && t.buf.capacity() <= 1000);
        }
        let lines = t.to_lines();
        let want = &all[all.len() - lines.len()..];
        assert!(!lines.is_empty());
        assert!(
            lines
                .iter()
                .zip(want)
                .all(|(a, b)| a.as_ref() == b.as_slice())
        );
        // A line longer than the limit empties it.
        t.push(&[b'x'; 1001], 1000);
        assert_eq!(t.lines(), 0);
        assert!(t.to_lines().is_empty());
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
        assert!(disk <= 2000, "disk {disk}");
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

    fn seg_len(dir: &Path, first: u64) -> u64 {
        fs::metadata(seg_path(dir, first)).unwrap().len()
    }

    fn assert_labelled(p: &Page) {
        for (i, l) in p.lines.iter().enumerate() {
            assert_eq!(
                l.as_ref(),
                line(p.first_line + i as u64).as_slice(),
                "mislabelled line"
            );
        }
    }

    #[test]
    fn torn_tail_is_truncated_on_reopen() {
        use std::io::Write;
        for tail in [vec![1u8, 2], {
            let mut v = 50u32.to_le_bytes().to_vec();
            v.extend_from_slice(b"partial!!!");
            v
        }] {
            let d = tempfile::tempdir().unwrap();
            let dir = d.path().join("h");
            let mut h = History::create(&dir).unwrap();
            fill(&mut h, 100);
            drop(h);
            let good = seg_len(&dir, 0);
            let mut f = OpenOptions::new()
                .append(true)
                .open(seg_path(&dir, 0))
                .unwrap();
            f.write_all(&tail).unwrap();
            drop(f);
            assert_eq!(seg_len(&dir, 0), good + tail.len() as u64);

            let mut h = History::open(&dir).unwrap();
            assert_eq!(h.info().next_line, 100);
            assert_eq!(seg_len(&dir, 0), good);
            let p = h.read_page(0, 100, 100).unwrap();
            assert_eq!(p.lines.len(), 100);
            assert_labelled(&p);
            // Appending after recovery lands at the right offset.
            h.push(&line(100));
            h.flush().unwrap();
            let p = h.read_page(0, 101, 3).unwrap();
            assert_eq!(p.first_line, 98);
            assert_labelled(&p);
        }
    }

    #[test]
    fn corrupt_length_is_an_error_not_an_allocation() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::create(&dir).unwrap();
        fill(&mut h, 10);
        h.flush().unwrap();
        OpenOptions::new()
            .write(true)
            .open(seg_path(&dir, 0))
            .unwrap()
            .write_all_at(&u32::MAX.to_le_bytes(), 0)
            .unwrap();
        assert!(h.read_page(0, 10, 10).is_err());
    }

    #[test]
    fn write_fault_never_returns_mislabelled_lines() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::create(&dir).unwrap();
        fill(&mut h, 100);
        h.flush().unwrap();
        let good = seg_len(&dir, 0);
        h.fault = true;
        for i in 100..150 {
            h.push(&line(i));
        }
        // Public flush fails and rolls accounting back to the durable state.
        assert!(h.flush().is_err());
        h.fault = false;
        for i in 150..200 {
            h.push(&line(i));
        }
        assert_eq!(h.info().next_line, 200);
        assert_eq!(seg_len(&dir, 0), good);
        // Durable range still reads correctly.
        let p = h.read_page(0, 100, 60).unwrap();
        assert_eq!((p.first_line, p.lines.len()), (40, 60));
        assert_labelled(&p);
        // Anything reaching past it errors instead of mislabelling.
        assert!(h.read_page(0, 150, 60).is_err());
        assert!(h.read_page(0, 200, 60).is_err());
        // The tail still serves the recent lines, correctly labelled.
        let (first, lines) = h.tail();
        assert_eq!(first + lines.len() as u64, 200);
        assert_eq!(lines[0].as_ref(), line(first).as_slice());
        // Reopen sees only what is durable.
        drop(h);
        let h = History::open(&dir).unwrap();
        assert_eq!(h.info().next_line, 100);
        assert_labelled(&h.read_page(0, 100, 100).unwrap());
    }

    #[test]
    fn write_fault_during_push_flush_is_consistent() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("h");
        let mut h = History::create(&dir).unwrap();
        fill(&mut h, 100);
        h.flush().unwrap();
        h.fault = true;
        // Overflow the 64 KiB buffer so the flush happens inside push.
        for i in 100..20_000 {
            h.push(&line(i));
        }
        assert_eq!(h.info().next_line, 20_000);
        assert!(h.read_page(0, 20_000, 60).is_err());
        let p = h.read_page(0, 100, 60).unwrap();
        assert_labelled(&p);
        // clear() recovers the store.
        h.fault = false;
        h.clear();
        h.push(&line(20_000));
        let p = h.read_page(1, 20_001, 5).unwrap();
        assert_eq!(p.first_line, 20_000);
        assert_labelled(&p);
    }
}
