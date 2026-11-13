//! Blocking protocol client: handshake, request/response and frame streaming
//! over any byte stream (a unix socket, an `ssh host fleetd --stdio` child, ...).

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotRunning => write!(f, "fleetd is not running"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Server(m) => write!(f, "{m}"),
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

impl Connection {
    /// Handshakes over an already-open byte stream.
    pub fn over(read: Box<dyn Read + Send>, write: Box<dyn Write + Send>) -> Result<Connection> {
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
                    client: "fleet".into(),
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

    /// Connects to the unix socket at `path` and handshakes.
    pub fn unix(path: &Path) -> Result<Connection> {
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
        Connection::over(Box::new(stream.try_clone()?), Box::new(stream))
    }

    /// Spawns `cmd` and talks over its stdin/stdout (e.g. `ssh host fleetd
    /// --stdio`). The child is returned so the caller can reap or kill it.
    pub fn command(mut cmd: Command) -> Result<(Connection, Child)> {
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        match Connection::over(Box::new(stdout), Box::new(stdin)) {
            Ok(conn) => Ok((conn, child)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Sends `kind` and waits for its response; unrelated frames (exit events
    /// for other agents) are skipped. A server error becomes `Error::Server`.
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
