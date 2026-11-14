use bytes::{Bytes, BytesMut};
use fleet_proto::{
    Ack, AgentActivity, AgentExited, AgentInfo, AgentList, AgentResized, AgentState, Attach,
    AttachOk, AttachRole, Detach, Error, Event, Frame, FrameCodec, HandshakeReject, Hello, HelloOk,
    HistoryPage, KillAgent, Lagged, ListAgents, PROTOCOL_VERSION, ReadHistory, Request, Resize,
    Response, SendInput, SetLabels, SpawnAgent, SpawnedAttached, event, request, response,
};
use tokio_util::codec::{Decoder, Encoder};

fn roundtrip(frame: Frame) -> Frame {
    let mut buf = BytesMut::new();
    FrameCodec::new().encode(frame, &mut buf).unwrap();
    FrameCodec::new().decode(&mut buf).unwrap().unwrap()
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
        attach: true,
        labels: [("project".to_string(), "fleetd".to_string())].into(),
    };
    let f = Frame::Request(Request {
        id: 1,
        kind: Some(request::Kind::Spawn(Box::new(spawn))),
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
            kind: Some(response::Kind::Spawned(Box::new(info))),
        });
        assert_eq!(roundtrip(f.clone()), f);
    }
}

#[test]
fn protocol_version_is_two() {
    assert_eq!(PROTOCOL_VERSION, 2);
}

fn labels() -> std::collections::HashMap<String, String> {
    [
        ("project".to_string(), "fleetd".to_string()),
        ("role".to_string(), "coder".to_string()),
    ]
    .into()
}

fn info() -> AgentInfo {
    AgentInfo {
        id: 7,
        program: "claude".into(),
        args: vec!["-p".into()],
        cwd: "/w".into(),
        state: AgentState::Running as i32,
        exit_code: None,
        created_unix_ms: 1,
        labels: labels(),
    }
}

fn attach_ok() -> AttachOk {
    AttachOk {
        agent_id: 7,
        snapshot: b"\x1b[Hscreen".to_vec(),
        cols: 80,
        rows: 24,
        history_tail: vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")],
        tail_first_line: 10,
        history_first_line: 3,
        history_generation: 2,
        primary: true,
    }
}

#[test]
fn roundtrip_every_kind() {
    let requests = [
        request::Kind::Spawn(Box::new(SpawnAgent {
            program: "p".into(),
            attach: true,
            labels: labels(),
            ..Default::default()
        })),
        request::Kind::List(ListAgents { selector: labels() }),
        request::Kind::Attach(Attach {
            agent_id: 1,
            observe: true,
        }),
        request::Kind::Detach(Detach { agent_id: 1 }),
        request::Kind::Input(SendInput {
            agent_id: 1,
            data: b"x".to_vec(),
        }),
        request::Kind::Resize(Resize {
            agent_id: 1,
            cols: 100,
            rows: 30,
        }),
        request::Kind::Kill(KillAgent { agent_id: 1 }),
        request::Kind::ReadHistory(ReadHistory {
            agent_id: 1,
            generation: 2,
            before_line: 300,
            max_lines: 50,
        }),
        request::Kind::SetLabels(SetLabels {
            agent_id: 1,
            set: labels(),
            remove: vec!["old".into()],
        }),
    ];
    let responses = [
        response::Kind::Spawned(Box::new(info())),
        response::Kind::Agents(AgentList {
            agents: vec![info(), info()],
            ..Default::default()
        }),
        response::Kind::Attached(Box::new(attach_ok())),
        response::Kind::Ack(Ack {}),
        response::Kind::Error(Error {
            message: "nope".into(),
        }),
        response::Kind::History(HistoryPage {
            agent_id: 1,
            first_line: 250,
            lines: vec![Bytes::from_static(b"a"), Bytes::from_static(b"\x1b[31mb")],
        }),
        response::Kind::SpawnedAttached(Box::new(SpawnedAttached {
            agent: Some(info()),
            attached: Some(attach_ok()),
        })),
    ];
    let events = [
        event::Kind::Exited(AgentExited {
            agent_id: 1,
            exit_code: -9,
        }),
        event::Kind::Lagged(Lagged { agent_id: 1 }),
        event::Kind::Activity(AgentActivity {
            agent_id: 1,
            last_line: "building...".into(),
            last_output_unix_ms: 1_700_000_000_000,
        }),
        event::Kind::Role(AttachRole {
            agent_id: 1,
            primary: true,
        }),
        event::Kind::AgentResized(AgentResized {
            agent_id: 1,
            cols: 100,
            rows: 30,
        }),
    ];
    let mut frames = vec![
        Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "c".into(),
        }),
        Frame::HelloOk(HelloOk {
            protocol_version: PROTOCOL_VERSION,
            server: "s".into(),
        }),
        Frame::Reject(HandshakeReject { reason: "r".into() }),
        Frame::Output {
            agent_id: 1,
            data: Bytes::from_static(b"out"),
        },
    ];
    frames.extend(requests.into_iter().enumerate().map(|(i, kind)| {
        Frame::Request(Request {
            id: i as u64 + 1,
            kind: Some(kind),
        })
    }));
    frames.extend(responses.into_iter().enumerate().map(|(i, kind)| {
        Frame::Response(Response {
            id: i as u64 + 1,
            kind: Some(kind),
        })
    }));
    frames.extend(
        events
            .into_iter()
            .map(|kind| Frame::Event(Event { kind: Some(kind) })),
    );
    for f in frames {
        assert_eq!(roundtrip(f.clone()), f);
    }
}

/// Frames are moved and initialised per decode; keep the enum no bigger than
/// it was at protocol v1 (136 bytes on 64-bit).
#[test]
fn frame_is_small() {
    assert!(std::mem::size_of::<fleet_proto::Frame>() <= 136);
}
