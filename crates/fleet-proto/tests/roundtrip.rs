use bytes::BytesMut;
use fleet_proto::{
    AgentInfo, AgentState, Frame, FrameCodec, PROTOCOL_VERSION, Request, Response, SpawnAgent,
    request, response,
};
use tokio_util::codec::{Decoder, Encoder};

fn roundtrip(frame: Frame) -> Frame {
    let mut buf = BytesMut::new();
    FrameCodec.encode(frame, &mut buf).unwrap();
    FrameCodec.decode(&mut buf).unwrap().unwrap()
}

#[test]
fn spawn_request_roundtrips_with_env() {
    let spawn = SpawnAgent {
        program: "claude".into(),
        args: vec!["--help".into()],
        cwd: "/tmp".into(),
        env: [("K".to_string(), "V".to_string())].into(),
        cols: 80,
        rows: 24,
    };
    let f = Frame::Request(Request {
        id: 1,
        kind: Some(request::Kind::Spawn(spawn)),
    });
    assert_eq!(roundtrip(f.clone()), f);
}

#[test]
fn agent_info_optional_exit_code() {
    for exit_code in [None, Some(0), Some(-9)] {
        let info = AgentInfo {
            id: 1,
            program: "p".into(),
            state: AgentState::Exited as i32,
            exit_code,
            created_unix_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let f = Frame::Response(Response {
            id: 2,
            kind: Some(response::Kind::Spawned(info)),
        });
        assert_eq!(roundtrip(f.clone()), f);
    }
}

#[test]
fn protocol_version_is_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}
