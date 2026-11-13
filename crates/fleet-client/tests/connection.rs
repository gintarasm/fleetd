use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use bytes::BytesMut;
use fleet_client::Connection;
use fleet_proto::{Ack, Frame, FrameCodec, HelloOk, PROTOCOL_VERSION, Response, request, response};
use tokio_util::codec::{Decoder, Encoder};

fn read_frame(s: &mut UnixStream, buf: &mut BytesMut) -> Frame {
    let mut codec = FrameCodec::new();
    loop {
        if let Some(f) = codec.decode(buf).unwrap() {
            return f;
        }
        let mut chunk = [0u8; 4096];
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "closed");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn write_frame(s: &mut UnixStream, f: Frame) {
    let mut out = BytesMut::new();
    FrameCodec::new().encode(f, &mut out).unwrap();
    s.write_all(&out).unwrap();
}

#[test]
fn connection_over_pipe_pair() {
    let (client_side, mut server) = UnixStream::pair().unwrap();
    let srv = std::thread::spawn(move || {
        let mut buf = BytesMut::new();
        match read_frame(&mut server, &mut buf) {
            Frame::Hello(h) => {
                assert_eq!(h.protocol_version, PROTOCOL_VERSION);
                assert_eq!(h.client, "pane-3");
            }
            _ => panic!("expected hello"),
        }
        write_frame(&mut server, Frame::HelloOk(HelloOk::default()));
        let id = match read_frame(&mut server, &mut buf) {
            Frame::Request(r) => {
                assert!(matches!(r.kind, Some(request::Kind::List(_))));
                r.id
            }
            _ => panic!("expected request"),
        };
        write_frame(
            &mut server,
            Frame::Response(Response {
                id,
                kind: Some(response::Kind::Ack(Ack {})),
            }),
        );
    });
    let mut conn = Connection::over(
        Box::new(client_side.try_clone().unwrap()),
        Box::new(client_side),
        "pane-3",
    )
    .unwrap();
    let reply = conn.call(request::Kind::List(Default::default())).unwrap();
    assert!(matches!(reply, response::Kind::Ack(_)));
    srv.join().unwrap();
}
