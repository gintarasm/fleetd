//! Length-prefixed frame codec: `[u32 BE len][u8 kind][payload]`.

use std::io;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;
use tokio_util::codec::{Decoder, Encoder};

use crate::{Event, HandshakeReject, Hello, HelloOk, MAX_FRAME, Request, Response};

const KIND_HELLO: u8 = 1;
const KIND_HELLO_OK: u8 = 2;
const KIND_REJECT: u8 = 3;
const KIND_REQUEST: u8 = 4;
const KIND_RESPONSE: u8 = 5;
const KIND_EVENT: u8 = 6;
const KIND_OUTPUT: u8 = 7;

/// A single message on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello(Hello),
    HelloOk(HelloOk),
    Reject(HandshakeReject),
    Request(Request),
    Response(Response),
    Event(Event),
    /// Raw terminal output for an agent; deliberately not protobuf.
    Output {
        agent_id: u64,
        data: Bytes,
    },
}

/// Errors produced while encoding or decoding frames.
#[derive(Debug)]
pub enum FrameError {
    TooLarge(usize),
    UnknownKind(u8),
    Decode(prost::DecodeError),
    Io(io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the length limit"),
            Self::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            Self::Decode(e) => write!(f, "protobuf decode error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<prost::DecodeError> for FrameError {
    fn from(e: prost::DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// Codec for [`Frame`]s over a byte stream.
///
/// Decoding rejects frames longer than the codec's limit ([`MAX_FRAME`] by
/// default; see [`FrameCodec::with_max`]). Encoding always allows up to
/// [`MAX_FRAME`].
#[derive(Debug, Clone, Copy)]
pub struct FrameCodec {
    max_len: usize,
}

impl FrameCodec {
    /// A codec accepting frames up to [`MAX_FRAME`].
    pub fn new() -> Self {
        Self { max_len: MAX_FRAME }
    }

    /// A codec decoding only frames whose length prefix is at most `max_len`
    /// (capped at [`MAX_FRAME`]), e.g. a small limit for the handshake.
    pub fn with_max(max_len: usize) -> Self {
        Self {
            max_len: max_len.min(MAX_FRAME),
        }
    }

    /// Changes the decode limit (capped at [`MAX_FRAME`]).
    pub fn set_max(&mut self, max_len: usize) {
        self.max_len = max_len.min(MAX_FRAME);
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new()
    }
}

fn put_msg(dst: &mut BytesMut, kind: u8, msg: &impl Message) -> Result<(), FrameError> {
    let len = 1 + msg.encoded_len();
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    dst.reserve(4 + len);
    dst.put_u32(len as u32);
    dst.put_u8(kind);
    msg.encode(dst).expect("capacity reserved");
    Ok(())
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), FrameError> {
        match frame {
            Frame::Hello(m) => put_msg(dst, KIND_HELLO, &m),
            Frame::HelloOk(m) => put_msg(dst, KIND_HELLO_OK, &m),
            Frame::Reject(m) => put_msg(dst, KIND_REJECT, &m),
            Frame::Request(m) => put_msg(dst, KIND_REQUEST, &m),
            Frame::Response(m) => put_msg(dst, KIND_RESPONSE, &m),
            Frame::Event(m) => put_msg(dst, KIND_EVENT, &m),
            Frame::Output { agent_id, data } => {
                let len = 1 + 8 + data.len();
                if len > MAX_FRAME {
                    return Err(FrameError::TooLarge(len));
                }
                dst.reserve(4 + len);
                dst.put_u32(len as u32);
                dst.put_u8(KIND_OUTPUT);
                dst.put_u64(agent_id);
                dst.put_slice(&data);
                Ok(())
            }
        }
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > self.max_len {
            return Err(FrameError::TooLarge(len));
        }
        if len == 0 {
            // A frame must at least carry its kind byte.
            return Err(FrameError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty frame",
            )));
        }
        if src.len() < 4 + len {
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let mut body = src.split_to(len).freeze();
        let kind = body.get_u8();
        let frame = match kind {
            KIND_HELLO => Frame::Hello(Hello::decode(body)?),
            KIND_HELLO_OK => Frame::HelloOk(HelloOk::decode(body)?),
            KIND_REJECT => Frame::Reject(HandshakeReject::decode(body)?),
            KIND_REQUEST => Frame::Request(Request::decode(body)?),
            KIND_RESPONSE => Frame::Response(Response::decode(body)?),
            KIND_EVENT => Frame::Event(Event::decode(body)?),
            KIND_OUTPUT => {
                if body.len() < 8 {
                    return Err(FrameError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "output frame shorter than agent id",
                    )));
                }
                let agent_id = body.get_u64();
                Frame::Output {
                    agent_id,
                    data: body,
                }
            }
            other => return Err(FrameError::UnknownKind(other)),
        };
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attach, request, response};

    fn every_frame() -> Vec<Frame> {
        vec![
            Frame::Hello(Hello {
                protocol_version: 1,
                client: "cli".into(),
                ..Default::default()
            }),
            Frame::HelloOk(HelloOk {
                protocol_version: 1,
                server: "fleetd".into(),
                ..Default::default()
            }),
            Frame::Reject(HandshakeReject {
                reason: "version".into(),
            }),
            Frame::Request(Request {
                id: 7,
                kind: Some(request::Kind::Attach(Attach {
                    agent_id: 3,
                    observe: false,
                })),
            }),
            Frame::Response(Response {
                id: 7,
                kind: Some(response::Kind::Ack(crate::Ack {})),
            }),
            Frame::Event(Event {
                kind: Some(crate::event::Kind::Exited(crate::AgentExited {
                    agent_id: 3,
                    exit_code: -1,
                })),
            }),
            Frame::Output {
                agent_id: 9,
                data: Bytes::from_static(b"\x1b[0mhi"),
            },
        ]
    }

    #[test]
    fn roundtrip_every_kind() {
        for frame in every_frame() {
            let mut buf = BytesMut::new();
            FrameCodec::new().encode(frame.clone(), &mut buf).unwrap();
            let got = FrameCodec::new().decode(&mut buf).unwrap().unwrap();
            assert_eq!(got, frame);
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn restart_and_forget_frames_round_trip() {
        let frames = vec![
            Frame::Request(Request {
                id: 1,
                kind: Some(request::Kind::Restart(crate::RestartAgent {
                    agent_id: 5,
                    cols: 80,
                    rows: 24,
                    attach: true,
                })),
            }),
            Frame::Event(Event {
                kind: Some(crate::event::Kind::AgentRemoved(crate::AgentRemoved {
                    agent_id: 5,
                    replaced_by: Some(9),
                })),
            }),
            Frame::Response(Response {
                id: 2,
                kind: Some(response::Kind::Agents(crate::AgentList {
                    agents: vec![],
                    replaced: [(5u64, 9u64)].into_iter().collect(),
                })),
            }),
            Frame::Response(Response {
                id: 1,
                kind: Some(response::Kind::Restarted(Box::new(crate::Restarted {
                    old_id: 5,
                    agent: Some(crate::AgentInfo {
                        id: 9,
                        program: "claude".into(),
                        ..Default::default()
                    }),
                    attached: Some(crate::AttachOk {
                        agent_id: 9,
                        cols: 80,
                        rows: 24,
                        ..Default::default()
                    }),
                }))),
            }),
            Frame::Request(Request {
                id: 3,
                kind: Some(request::Kind::Forget(Box::new(crate::ForgetAgents {
                    ids: vec![],
                    exited_only: true,
                    selector: [("project".to_string(), "x".to_string())]
                        .into_iter()
                        .collect(),
                }))),
            }),
            Frame::Response(Response {
                id: 3,
                kind: Some(response::Kind::Forgotten(crate::Forgotten {
                    ids: vec![5, 6],
                    refused: vec![7],
                })),
            }),
        ];
        for frame in frames {
            let mut buf = BytesMut::new();
            FrameCodec::new().encode(frame.clone(), &mut buf).unwrap();
            let got = FrameCodec::new().decode(&mut buf).unwrap().unwrap();
            assert_eq!(got, frame);
            assert!(buf.is_empty());
        }
    }

    /// `Frame` is moved on every decode (the `frame_decode` benchmark): a
    /// large new oneof variant must be boxed in `build.rs`, not grow it.
    #[test]
    fn frame_stays_small() {
        assert!(
            std::mem::size_of::<Frame>() <= 88,
            "Frame is {} bytes: box the large variant in build.rs",
            std::mem::size_of::<Frame>()
        );
    }

    #[test]
    fn output_frame_is_raw() {
        let mut buf = BytesMut::new();
        let frame = Frame::Output {
            agent_id: 0x0102,
            data: Bytes::from_static(b"abc"),
        };
        FrameCodec::new().encode(frame, &mut buf).unwrap();
        assert_eq!(buf.len(), 4 + 1 + 8 + 3);
        assert_eq!(&buf[..4], &12u32.to_be_bytes());
        assert_eq!(buf[4], 7);
        assert_eq!(&buf[5..13], &0x0102u64.to_be_bytes());
        assert_eq!(&buf[13..], b"abc");
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME + 1) as u32);
        let before = buf.capacity();
        assert!(matches!(
            FrameCodec::new().decode(&mut buf),
            Err(FrameError::TooLarge(n)) if n == MAX_FRAME + 1
        ));
        assert_eq!(
            buf.capacity(),
            before,
            "must not reserve for oversized frame"
        );
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u32(1);
        buf.put_u8(99);
        assert!(matches!(
            FrameCodec::new().decode(&mut buf),
            Err(FrameError::UnknownKind(99))
        ));
    }

    #[test]
    fn partial_frames_buffer() {
        for frame in [
            Frame::Request(Request {
                id: 1,
                kind: Some(request::Kind::Attach(Attach {
                    agent_id: 2,
                    observe: false,
                })),
            }),
            Frame::Output {
                agent_id: 5,
                data: Bytes::from_static(b"xyz"),
            },
        ] {
            let mut wire = BytesMut::new();
            FrameCodec::new().encode(frame.clone(), &mut wire).unwrap();
            let mut buf = BytesMut::new();
            let last = wire.len() - 1;
            for (i, b) in wire.iter().enumerate() {
                buf.put_u8(*b);
                let got = FrameCodec::new().decode(&mut buf).unwrap();
                if i < last {
                    assert!(got.is_none(), "frame yielded early at byte {i}");
                } else {
                    assert_eq!(got, Some(frame.clone()));
                }
            }
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn garbage_protobuf_is_decode_error() {
        let mut buf = BytesMut::new();
        buf.put_u32(4);
        buf.put_u8(KIND_REQUEST);
        buf.put_slice(&[0xff, 0xff, 0xff]);
        assert!(matches!(
            FrameCodec::new().decode(&mut buf),
            Err(FrameError::Decode(_))
        ));
    }

    #[test]
    fn zero_length_frame_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u32(0);
        assert!(matches!(
            FrameCodec::new().decode(&mut buf),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn short_output_payload_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u32(1 + 7);
        buf.put_u8(KIND_OUTPUT);
        buf.put_slice(&[0; 7]);
        assert!(matches!(
            FrameCodec::new().decode(&mut buf),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn oversized_output_encode_rejected() {
        let data = Bytes::from(vec![0u8; MAX_FRAME]);
        let mut buf = BytesMut::new();
        let res = FrameCodec::new().encode(Frame::Output { agent_id: 1, data }, &mut buf);
        assert!(matches!(res, Err(FrameError::TooLarge(n)) if n == MAX_FRAME + 9));
        assert!(buf.is_empty());
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let frames = every_frame();
        let mut buf = BytesMut::new();
        FrameCodec::new()
            .encode(frames[0].clone(), &mut buf)
            .unwrap();
        FrameCodec::new()
            .encode(frames[6].clone(), &mut buf)
            .unwrap();
        assert_eq!(
            FrameCodec::new().decode(&mut buf).unwrap(),
            Some(frames[0].clone())
        );
        assert!(!buf.is_empty());
        assert_eq!(
            FrameCodec::new().decode(&mut buf).unwrap(),
            Some(frames[6].clone())
        );
        assert!(buf.is_empty());
        assert_eq!(FrameCodec::new().decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn max_frame_length_accepted_by_header_check() {
        let mut buf = BytesMut::new();
        buf.put_u32(MAX_FRAME as u32);
        buf.put_u8(KIND_OUTPUT);
        assert!(FrameCodec::new().decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn custom_limit_rejects_longer_frames() {
        let mut codec = FrameCodec::with_max(4096);
        let mut buf = BytesMut::new();
        buf.put_u32(4097);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::TooLarge(4097))
        ));

        let mut buf = BytesMut::new();
        buf.put_u32(4096);
        buf.put_u8(KIND_OUTPUT);
        assert!(codec.decode(&mut buf).unwrap().is_none());

        codec.set_max(usize::MAX);
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME + 1) as u32);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }
}
