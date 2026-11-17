use bytes::{Bytes, BytesMut};
use fleet_proto::{
    Ack, AgentActivity, AgentExited, AgentInfo, AgentList, AgentRemoved, AgentReported,
    AgentResized, AgentState, AnswerQuestion, Attach, AttachOk, AttachRole, Detach, Error, Event,
    ForgetAgents, Forgotten, Frame, FrameCodec, HandshakeReject, Hello, HelloOk, HistoryPage,
    KillAgent, Lagged, ListAgents, PROTOCOL_VERSION, ReadHistory, ReportStatus, Reported,
    ReportedState, Request, Resize, Response, RestartAgent, Restarted, SendInput, SetLabels,
    SpawnAgent, SpawnedAttached, event, request, response,
};
use fleet_proto::{
    Broadcast, Inbox, Message, MessageKind, MessagesSent, ReadInbox, SendMessage, Wait, WaitMatch,
    WaitResult, WaitState,
};
use tokio_util::codec::{Decoder, Encoder};

fn roundtrip(frame: Frame) -> Frame {
    let mut buf = BytesMut::new();
    FrameCodec::new().encode(frame, &mut buf).unwrap();
    let got = FrameCodec::new().decode(&mut buf).unwrap().unwrap();
    assert!(buf.is_empty(), "decode left {} bytes", buf.len());
    got
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
        priority: fleet_proto::AgentPriority::Normal.into(),
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
        exited_unix_ms: Some(2),
        reported: Some(Reported {
            state: ReportedState::ReportedWaiting.into(),
            message: "need a review".into(),
            at_unix_ms: 3,
            question: Some(fleet_proto::Question {
                id: 5,
                text: "Which branch?".into(),
                choices: vec!["main".into(), "dev".into()],
            }),
        }),
        git: Some(fleet_proto::GitInfo {
            repo: "fleetd".into(),
            worktree: "issue-97".into(),
            branch: "feat/gui-git-branch".into(),
            detached: false,
        }),
    }
}

/// ADR 0085: `AgentInfo.git` is additive. A message from an older node
/// (no field 10) reads as no checkout, and an older client skips it.
#[test]
fn agent_info_git_is_optional_and_skippable() {
    use prost::Message;
    let mut without = info();
    without.git = None;
    let bytes = without.encode_to_vec();
    assert_eq!(AgentInfo::decode(bytes.as_slice()).unwrap().git, None);
    // An older client's AgentInfo: fields 1-9 only.
    #[derive(Clone, PartialEq, prost::Message)]
    struct OldAgentInfo {
        #[prost(uint64, tag = "1")]
        id: u64,
        #[prost(string, tag = "4")]
        cwd: String,
    }
    let old = OldAgentInfo::decode(info().encode_to_vec().as_slice()).unwrap();
    assert_eq!((old.id, old.cwd.as_str()), (7, "/w"));
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
        request::Kind::Restart(RestartAgent {
            agent_id: 1,
            cols: 80,
            rows: 24,
            attach: true,
        }),
        request::Kind::Forget(Box::new(ForgetAgents {
            ids: vec![1, 2],
            exited_only: false,
            selector: labels(),
        })),
        request::Kind::Report(Box::new(ReportStatus {
            state: ReportedState::ReportedDone.into(),
            message: "tests green".into(),
            ..Default::default()
        })),
        request::Kind::Report(Box::new(ReportStatus {
            state: ReportedState::ReportedWaiting.into(),
            message: "stuck".into(),
            question: Some("Which branch?".into()),
            choices: vec!["main".into(), "dev".into()],
        })),
        request::Kind::Answer(AnswerQuestion {
            agent_id: 3,
            text: "dev".into(),
            question_id: Some(9),
        }),
        request::Kind::SendMessage(Box::new(SendMessage {
            body: "hi".into(),
            agent_ids: vec![3],
            to_user: true,
            to_parent: true,
            broadcast: Broadcast::Descendants as i32,
            selector: labels(),
        })),
        request::Kind::ReadInbox(ReadInbox {
            since: Some(0),
            max: 10,
        }),
        request::Kind::Wait(Box::new(Wait {
            agent_ids: vec![1, 2],
            states: vec![WaitState::Exited as i32, WaitState::Message as i32],
            timeout_ms: 600_000,
        })),
    ];
    let message = Message {
        id: 7,
        sender: Some(3),
        recipient: None,
        kind: MessageKind::Exit as i32,
        body: "exited with code 2".into(),
        created_unix_ms: 1_700_000_000_000,
        sender_name: "worker".into(),
        status: String::new(),
        exit_code: Some(2),
        choices: vec!["a".into()],
    };
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
        response::Kind::Restarted(Box::new(Restarted {
            old_id: 1,
            agent: Some(info()),
            attached: Some(attach_ok()),
        })),
        response::Kind::Forgotten(Forgotten {
            ids: vec![1],
            refused: vec![2, 3],
        }),
        response::Kind::MessagesSent(Box::new(MessagesSent {
            agent_ids: vec![3, 4],
            user: true,
        })),
        response::Kind::Inbox(Box::new(Inbox {
            messages: vec![message.clone()],
            cursor: 7,
        })),
        response::Kind::WaitResult(Box::new(WaitResult {
            matches: vec![WaitMatch {
                agent_id: Some(3),
                state: WaitState::Exited as i32,
                message: Some(message),
            }],
            agents: vec![info()],
        })),
    ];
    let events = [
        event::Kind::Reported(Box::new(AgentReported {
            agent_id: 1,
            reported: Some(Reported {
                state: ReportedState::ReportedError.into(),
                message: "build broke".into(),
                at_unix_ms: 9,
                question: None,
            }),
        })),
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
        event::Kind::AgentRemoved(AgentRemoved {
            agent_id: 1,
            replaced_by: Some(2),
        }),
    ];
    let mut frames = vec![
        Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "c".into(),
            ..Default::default()
        }),
        Frame::HelloOk(HelloOk {
            protocol_version: PROTOCOL_VERSION,
            server: "s".into(),
            ..Default::default()
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

#[test]
fn restart_daemon_frames_round_trip() {
    let req = Frame::Request(Request {
        id: 1,
        kind: Some(request::Kind::RestartDaemon(fleet_proto::RestartDaemon {
            binary: "/x/fleetd".into(),
        })),
    });
    assert_eq!(roundtrip(req.clone()), req);

    let resp = Frame::Response(Response {
        id: 1,
        kind: Some(response::Kind::RestartingDaemon(
            fleet_proto::RestartingDaemon { agents: 3 },
        )),
    });
    assert_eq!(roundtrip(resp.clone()), resp);

    let ok = Frame::HelloOk(HelloOk {
        protocol_version: PROTOCOL_VERSION,
        server: "s".into(),
        instance: 9,
        binary: Some(Box::new(fleet_proto::BinaryIdentity {
            path: "/x/fleetd".into(),
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: -4,
        })),
        restore: Some(Box::new(fleet_proto::RestoreSummary {
            previous_instance: 8,
            kept: vec![1, 2],
            failed: vec![3],
            fell_back: true,
            restart_error: "boom".into(),
        })),
    });
    assert_eq!(roundtrip(ok.clone()), ok);
}

#[test]
fn handoff_record_round_trips() {
    use fleet_proto::handoff::{HandoffAgent, HandoffRecord, handoff_agent::Terminal};
    use prost::Message;
    let rec = HandoffRecord {
        version: fleet_proto::HANDOFF_VERSION,
        listener_fd: 3,
        lock_fd: 4,
        previous_instance: 7,
        frozen_unix_ms: 1,
        agents: vec![
            HandoffAgent {
                id: 1,
                pid: 100,
                cols: 80,
                rows: 24,
                snapshot: vec![1, 2, 3],
                terminal: Some(Terminal::Fd(7)),
            },
            HandoffAgent {
                id: 2,
                pid: 101,
                cols: 100,
                rows: 30,
                snapshot: vec![],
                terminal: Some(Terminal::HolderSocket("/s".into())),
            },
        ],
    };
    let got = HandoffRecord::decode(rec.encode_to_vec().as_slice()).unwrap();
    assert_eq!(got, rec);
}
