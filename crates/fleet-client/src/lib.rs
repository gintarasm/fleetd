//! Blocking protocol client: handshake, request/response and frame streaming
//! over any byte stream (a unix socket, an `ssh host fleetd --stdio` child, ...).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use bytes::BytesMut;
use fleet_proto::{
    Event, Frame, FrameCodec, Hello, PROTOCOL_VERSION, ReadLogs, Request, event, request, response,
};
use tokio_util::codec::{Decoder, Encoder};

#[derive(Debug)]
pub enum Error {
    NotRunning,
    Io(io::Error),
    Protocol(String),
    /// The server answered a request with an error.
    Server(String),
    /// The server did not complete the handshake within
    /// [`HANDSHAKE_TIMEOUT`].
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotRunning => write!(f, "fleetd is not running"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Server(m) => write!(f, "{m}"),
            Error::Timeout => write!(f, "fleetd did not answer the handshake in time"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Decodes frames from the socket.
pub struct FrameReader {
    stream: Box<dyn Read + Send>,
    buf: BytesMut,
}

impl FrameReader {
    /// The next frame; `Ok(None)` when the server closed the connection.
    pub fn read_frame(&mut self) -> Result<Option<Frame>> {
        let mut codec = FrameCodec::new();
        loop {
            if let Some(frame) = codec
                .decode(&mut self.buf)
                .map_err(|e| Error::Protocol(e.to_string()))?
            {
                return Ok(Some(frame));
            }
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Encodes requests onto the socket.
pub struct Sender {
    stream: Box<dyn Write + Send>,
    next_id: u64,
}

impl Sender {
    /// Sends a request and returns its id.
    pub fn send(&mut self, kind: request::Kind) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let mut out = BytesMut::new();
        FrameCodec::new()
            .encode(
                Frame::Request(Request {
                    id,
                    kind: Some(kind),
                }),
                &mut out,
            )
            .map_err(|e| Error::Protocol(e.to_string()))?;
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(id)
    }
}

pub struct Connection {
    reader: FrameReader,
    sender: Sender,
}

/// How long a constructor waits for the server's answer to its `Hello`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

impl Connection {
    /// Handshakes over an already-open byte stream as `client_name` (shown in
    /// the daemon's log), within [`HANDSHAKE_TIMEOUT`]. On a timeout the
    /// handshake thread keeps `read` and `write` until the peer closes the
    /// stream; prefer [`Connection::unix`] or [`Connection::command`], which
    /// close it.
    pub fn over(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        client_name: &str,
    ) -> Result<Connection> {
        Self::over_within(read, write, client_name, HANDSHAKE_TIMEOUT, || {})
    }

    /// Runs the handshake on its own thread, so a peer that never answers
    /// (or never reads) cannot block past `timeout`; `on_timeout` should
    /// close the stream so that thread ends.
    fn over_within(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        client_name: &str,
        timeout: Duration,
        on_timeout: impl FnOnce(),
    ) -> Result<Connection> {
        let (tx, rx) = mpsc::channel();
        let name = client_name.to_owned();
        thread::Builder::new()
            .name("fleet-handshake".into())
            .spawn(move || {
                let _ = tx.send(Self::handshake(read, write, &name));
            })?;
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                on_timeout();
                Err(Error::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::Protocol("handshake thread failed".into()))
            }
        }
    }

    fn handshake(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        client_name: &str,
    ) -> Result<Connection> {
        let mut conn = Connection {
            reader: FrameReader {
                stream: read,
                buf: BytesMut::new(),
            },
            sender: Sender {
                stream: write,
                next_id: 1,
            },
        };
        let mut out = BytesMut::new();
        FrameCodec::new()
            .encode(
                Frame::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    client: client_name.into(),
                }),
                &mut out,
            )
            .map_err(|e| Error::Protocol(e.to_string()))?;
        conn.sender.stream.write_all(&out)?;
        conn.sender.stream.flush()?;
        match conn.reader.read_frame()? {
            Some(Frame::HelloOk(_)) => Ok(conn),
            Some(Frame::Reject(r)) => Err(Error::Protocol(format!("rejected: {}", r.reason))),
            Some(_) => Err(Error::Protocol("unexpected reply to hello".into())),
            None => Err(Error::Protocol("closed during handshake".into())),
        }
    }

    /// Connects to the unix socket at `path` and handshakes as
    /// `client_name` within [`HANDSHAKE_TIMEOUT`] (closing the socket on a
    /// timeout).
    pub fn unix(path: &Path, client_name: &str) -> Result<Connection> {
        Self::unix_within(path, client_name, HANDSHAKE_TIMEOUT)
    }

    fn unix_within(path: &Path, client_name: &str, timeout: Duration) -> Result<Connection> {
        let stream = match UnixStream::connect(path) {
            Ok(s) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(Error::NotRunning);
            }
            Err(e) => return Err(e.into()),
        };
        let closer = stream.try_clone()?;
        Self::over_within(
            Box::new(stream.try_clone()?),
            Box::new(stream),
            client_name,
            timeout,
            move || {
                let _ = closer.shutdown(Shutdown::Both);
            },
        )
    }

    /// Spawns `cmd` and talks over its stdin/stdout (e.g. `ssh host fleetd
    /// --stdio`), handshaking as `client_name` within
    /// [`HANDSHAKE_TIMEOUT`]. The child is returned so the caller can reap or
    /// kill it; on any failure (a timeout included) it is killed and reaped.
    pub fn command(cmd: Command, client_name: &str) -> Result<(Connection, Child)> {
        Self::command_within(cmd, client_name, HANDSHAKE_TIMEOUT)
    }

    fn command_within(
        mut cmd: Command,
        client_name: &str,
        timeout: Duration,
    ) -> Result<(Connection, Child)> {
        let child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let child = Arc::new(Mutex::new(child));
        let (stdin, stdout) = {
            let mut c = lock(&child);
            (
                c.stdin.take().expect("piped stdin"),
                c.stdout.take().expect("piped stdout"),
            )
        };
        let killer = Arc::clone(&child);
        let r = Self::over_within(
            Box::new(stdout),
            Box::new(stdin),
            client_name,
            timeout,
            move || {
                // Ends the child, and with it the handshake thread's read.
                let _ = lock(&killer).kill();
            },
        );
        let mut child = match Arc::try_unwrap(child) {
            Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()),
            Err(_) => unreachable!("the timeout closure has run or been dropped"),
        };
        match r {
            Ok(conn) => Ok((conn, child)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Sends `kind` and waits for its response. Every other frame read
    /// meanwhile (agent output, every event) is discarded, so use it only on
    /// a connection that has not attached to anything, or before
    /// [`Connection::split`]; afterwards send with the [`Sender`] and match
    /// responses in the [`FrameReader`]'s stream. A server error becomes
    /// `Error::Server`.
    pub fn call(&mut self, kind: request::Kind) -> Result<response::Kind> {
        let id = self.sender.send(kind)?;
        loop {
            match self.reader.read_frame()? {
                Some(Frame::Response(r)) if r.id == id => {
                    return match r.kind {
                        Some(response::Kind::Error(e)) => Err(Error::Server(e.message)),
                        Some(kind) => Ok(kind),
                        None => Err(Error::Protocol("empty response".into())),
                    };
                }
                Some(_) => {}
                None => return Err(Error::Protocol("connection closed".into())),
            }
        }
    }

    /// The last `tail` lines of the daemon's log, oldest first (ADR 0027).
    /// With `follow`, new lines then arrive as `Event::LogLine` frames;
    /// read them with [`Connection::next_log_line`].
    pub fn read_logs(&mut self, tail: u32, follow: bool) -> Result<Vec<String>> {
        match self.call(request::Kind::ReadLogs(ReadLogs { tail, follow }))? {
            response::Kind::LogLines(l) => Ok(l.lines),
            _ => Err(Error::Protocol("unexpected reply to read logs".into())),
        }
    }

    /// The next followed log line, skipping other frames; `None` when the
    /// server closed the connection.
    pub fn next_log_line(&mut self) -> Result<Option<String>> {
        loop {
            match self.reader.read_frame()? {
                Some(Frame::Event(Event {
                    kind: Some(event::Kind::LogLine(l)),
                })) => return Ok(Some(l.line)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    pub fn split(self) -> (FrameReader, Sender) {
        (self.reader, self.sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    const SHORT: Duration = Duration::from_millis(300);

    /// Runs `f` on a thread and fails if it takes longer than 5 s.
    fn within_5s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> (T, Duration) {
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        let r = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("handshake did not time out");
        (r, start.elapsed())
    }

    fn is_timeout(r: &Result<impl Sized>) -> bool {
        matches!(r, Err(Error::Timeout))
    }

    #[test]
    fn handshake_times_out_over_a_silent_stream() {
        let (client, server) = UnixStream::pair().unwrap();
        let closer = client.try_clone().unwrap();
        let (r, took) = within_5s(move || {
            Connection::over_within(
                Box::new(client.try_clone().unwrap()),
                Box::new(client),
                "t",
                SHORT,
                move || {
                    let _ = closer.shutdown(Shutdown::Both);
                },
            )
        });
        assert!(is_timeout(&r), "{:?}", r.err());
        assert!(took >= SHORT);
        drop(server);
    }

    #[test]
    fn handshake_times_out_on_a_silent_unix_socket() {
        let dir = std::env::temp_dir().join(format!("fleet-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silent.sock");
        let _ = std::fs::remove_file(&path);
        // Accepts (via the backlog) but never answers.
        let listener = UnixListener::bind(&path).unwrap();
        let p = path.clone();
        let (r, _) = within_5s(move || Connection::unix_within(&p, "t", SHORT));
        assert!(is_timeout(&r), "{:?}", r.err());
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handshake_times_out_on_a_silent_command_and_kills_it() {
        let mut cmd = Command::new("/bin/sh");
        // Prints its pid, then never answers.
        cmd.args(["-c", "echo $$ >&2; exec sleep 30"]);
        let pidfile = std::env::temp_dir().join(format!("fleet-client-pid-{}", std::process::id()));
        cmd.stderr(std::fs::File::create(&pidfile).unwrap());
        let (r, _) = within_5s(move || Connection::command_within(cmd, "t", SHORT));
        assert!(is_timeout(&r), "{:?}", r.err().map(|e| e.to_string()));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(&pidfile);
        // Killed and reaped: the pid no longer exists.
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the silent child {pid} is still running");
    }
}
