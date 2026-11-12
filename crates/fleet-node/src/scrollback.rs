//! Bounded on-disk ring file holding an agent's output history.
//!
//! Layout: 16-byte header `[u64 LE write_pos][u64 LE total_written]` followed
//! by `capacity` data bytes. Appends accumulate in an in-memory pending buffer
//! and reach the file on `flush()`, drop, or when the buffer would overflow;
//! there is never an fsync. The header is rewritten on flush only.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const HEADER_LEN: u64 = 16;
const PENDING_MAX: usize = 64 * 1024;

/// Default ring capacity in bytes.
pub const DEFAULT_CAPACITY: u64 = 8 * 1024 * 1024;

pub struct Scrollback {
    file: File,
    path: PathBuf,
    capacity: u64,
    /// Ring position where the next flushed byte goes.
    write_pos: u64,
    /// Total bytes ever flushed to the file.
    flushed_total: u64,
    pending: Vec<u8>,
}

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Scrollback {
    /// Create (truncating) a ring file with the given capacity.
    pub fn create(path: &Path, capacity: u64) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scrollback capacity must be > 0",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(HEADER_LEN + capacity)?;
        let s = Scrollback {
            file,
            path: path.to_path_buf(),
            capacity,
            write_pos: 0,
            flushed_total: 0,
            pending: Vec::new(),
        };
        s.write_header()?;
        Ok(s)
    }

    /// Open an existing ring file, validating its header.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let file_len = file.metadata()?.len();
        if file_len <= HEADER_LEN {
            return Err(invalid_data("scrollback file too short"));
        }
        let capacity = file_len - HEADER_LEN;
        let mut hdr = [0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut hdr, 0)?;
        let write_pos = u64::from_le_bytes(hdr[..8].try_into().unwrap());
        let flushed_total = u64::from_le_bytes(hdr[8..].try_into().unwrap());
        if write_pos >= capacity {
            return Err(invalid_data("scrollback write_pos out of range"));
        }
        if flushed_total % capacity != write_pos {
            return Err(invalid_data("scrollback header inconsistent"));
        }
        Ok(Scrollback {
            file,
            path: path.to_path_buf(),
            capacity,
            write_pos,
            flushed_total,
            pending: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes retained (flushed + pending), at most the capacity.
    pub fn len(&self) -> u64 {
        (self.flushed_total + self.pending.len() as u64).min(self.capacity)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append bytes. Buffered: no syscall unless the pending buffer fills.
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.pending.len() + bytes.len() <= PENDING_MAX {
            self.pending.extend_from_slice(bytes);
            return Ok(());
        }
        self.flush_pending()?;
        if bytes.len() > PENDING_MAX {
            self.write_ring(bytes)?;
            self.write_header()
        } else {
            self.pending.extend_from_slice(bytes);
            Ok(())
        }
    }

    /// Write pending bytes to the file and update the header.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.flush_pending()?;
        self.write_header()
    }

    /// Most recent `max` bytes (fewer if less retained), oldest first.
    pub fn read_tail(&self, max: usize) -> io::Result<Vec<u8>> {
        let n = (max as u64).min(self.len()) as usize;
        let from_pending = n.min(self.pending.len());
        let from_disk = n - from_pending;
        let mut out = vec![0u8; n];
        if from_disk > 0 {
            let start = (self.write_pos + self.capacity - from_disk as u64) % self.capacity;
            let first = ((self.capacity - start) as usize).min(from_disk);
            self.file
                .read_exact_at(&mut out[..first], HEADER_LEN + start)?;
            if first < from_disk {
                self.file
                    .read_exact_at(&mut out[first..from_disk], HEADER_LEN)?;
            }
        }
        out[from_disk..].copy_from_slice(&self.pending[self.pending.len() - from_pending..]);
        Ok(out)
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // Take the buffer so write_ring can borrow self mutably; restore its
        // allocation afterwards. On error the bytes stay pending.
        let mut pending = std::mem::take(&mut self.pending);
        let res = self.write_ring(&pending);
        if res.is_ok() {
            pending.clear();
        }
        self.pending = pending;
        res
    }

    /// Write bytes at the ring position, keeping only the last `capacity`.
    fn write_ring(&mut self, bytes: &[u8]) -> io::Result<()> {
        let total_len = bytes.len() as u64;
        let skip = total_len.saturating_sub(self.capacity);
        let data = &bytes[skip as usize..];
        let start = (self.write_pos + skip) % self.capacity;
        let first = ((self.capacity - start) as usize).min(data.len());
        self.file.write_all_at(&data[..first], HEADER_LEN + start)?;
        if first < data.len() {
            self.file.write_all_at(&data[first..], HEADER_LEN)?;
        }
        self.write_pos = (self.write_pos + total_len) % self.capacity;
        self.flushed_total += total_len;
        Ok(())
    }

    fn write_header(&self) -> io::Result<()> {
        let mut hdr = [0u8; HEADER_LEN as usize];
        hdr[..8].copy_from_slice(&self.write_pos.to_le_bytes());
        hdr[8..].copy_from_slice(&self.flushed_total.to_le_bytes());
        self.file.write_all_at(&hdr, 0)
    }
}

impl Drop for Scrollback {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn sb(cap: u64) -> (tempfile::TempDir, Scrollback) {
        let dir = tempfile::tempdir().unwrap();
        let s = Scrollback::create(&dir.path().join("sb"), cap).unwrap();
        (dir, s)
    }

    #[test]
    fn tail_before_wrap() {
        let (_d, mut s) = sb(1000);
        s.append(b"hello ").unwrap();
        s.append(b"world").unwrap();
        assert_eq!(s.read_tail(1000).unwrap(), b"hello world");
        assert_eq!(s.read_tail(5).unwrap(), b"world");
        assert_eq!(s.len(), 11);
    }

    #[test]
    fn tail_after_wrap_is_most_recent() {
        let (_d, mut s) = sb(1000);
        let data = pattern(2500);
        for c in data.chunks(97) {
            s.append(c).unwrap();
        }
        assert_eq!(s.read_tail(1000).unwrap(), data[1500..]);
        s.flush().unwrap();
        assert_eq!(s.read_tail(1000).unwrap(), data[1500..]);
        assert_eq!(s.read_tail(10).unwrap(), data[2490..]);
    }

    #[test]
    fn chunk_larger_than_capacity_keeps_suffix() {
        let (_d, mut s) = sb(100);
        s.append(b"abc").unwrap();
        let data = pattern(70_000);
        s.append(&data).unwrap();
        assert_eq!(s.read_tail(100).unwrap(), data[69_900..]);
        s.append(b"xyz").unwrap();
        s.flush().unwrap();
        let mut want = data.clone();
        want.extend_from_slice(b"xyz");
        assert_eq!(s.read_tail(100).unwrap(), &want[want.len() - 100..]);
    }

    #[test]
    fn reopen_reads_same_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sb");
        let data = pattern(2500);
        {
            let mut s = Scrollback::create(&p, 1000).unwrap();
            s.append(&data).unwrap();
        }
        let mut s = Scrollback::open(&p).unwrap();
        assert_eq!(s.len(), 1000);
        assert_eq!(s.read_tail(1000).unwrap(), data[1500..]);
        s.append(b"zz").unwrap();
        assert_eq!(s.read_tail(2).unwrap(), b"zz");
    }

    #[test]
    fn len_never_exceeds_capacity() {
        let (_d, mut s) = sb(64);
        for _ in 0..50 {
            s.append(&[7u8; 13]).unwrap();
            assert!(s.len() <= 64);
        }
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn zero_capacity_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let e = Scrollback::create(&dir.path().join("sb"), 0).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn open_rejects_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sb");
        std::fs::write(&p, b"short").unwrap();
        assert_eq!(
            Scrollback::open(&p).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );

        let mut bytes = vec![0u8; 16 + 100];
        bytes[..8].copy_from_slice(&100u64.to_le_bytes()); // write_pos == capacity
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(
            Scrollback::open(&p).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );

        bytes[..8].copy_from_slice(&5u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&7u64.to_le_bytes()); // 7 % 100 != 5
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(
            Scrollback::open(&p).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn small_appends_do_not_touch_disk_until_flush() {
        let (_d, mut s) = sb(1000);
        s.append(b"abc").unwrap();
        let raw = std::fs::read(s.path()).unwrap();
        assert!(raw.iter().all(|b| *b == 0));
        s.flush().unwrap();
        let raw = std::fs::read(s.path()).unwrap();
        assert_eq!(&raw[16..19], b"abc");
    }
}
