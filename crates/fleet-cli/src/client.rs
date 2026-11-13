//! Blocking protocol client: connect, handshake, request/response.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use bytes::BytesMut;
use fleet_proto::{Frame, FrameCodec, Hello, PROTOCOL_VERSION, Request, request, response};
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
    stream: UnixStream,
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
    stream: UnixStream,
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
        Ok(id)
    }
}

pub struct Client {
    reader: FrameReader,
    sender: Sender,
}

impl Client {
    /// Connects to `<state_dir>/fleetd.sock` and completes the handshake.
    pub fn connect(state_dir: &Path) -> Result<Client> {
        let stream = match UnixStream::connect(state_dir.join("fleetd.sock")) {
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
        let mut client = Client {
            reader: FrameReader {
                stream: stream.try_clone()?,
                buf: BytesMut::new(),
            },
            sender: Sender { stream, next_id: 1 },
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
        client.sender.stream.write_all(&out)?;
        match client.reader.read_frame()? {
            Some(Frame::HelloOk(_)) => Ok(client),
            Some(Frame::Reject(r)) => Err(Error::Protocol(format!("rejected: {}", r.reason))),
            Some(_) => Err(Error::Protocol("unexpected reply to hello".into())),
            None => Err(Error::Protocol("closed during handshake".into())),
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

    pub fn split(self) -> (FrameReader, Sender) {
        (self.reader, self.sender)
    }
}
