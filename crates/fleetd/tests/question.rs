//! Questions and answers over the wire (ADR 0078): the question event is
//! scoped like every agent event, and only the user answers.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_client::Connection;
use fleet_proto::{
    AnswerQuestion, Frame, ListAgents, ReportStatus, ReportedState, SpawnAgent, event, request,
    response,
};

struct Daemon(Child);

impl Daemon {
    fn start(dir: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_fleetd"))
            .env("FLEETD_LOGIN_ENV", "off")
            .arg("--state-dir")
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fleetd");
        let sock = dir.join("fleetd.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon not ready within 5s");
            std::thread::sleep(Duration::from_millis(5));
        }
        Daemon(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(c: &mut Connection, script: &str) -> u64 {
    let req = SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        ..Default::default()
    };
    match c.call(request::Kind::Spawn(Box::new(req))) {
        Ok(response::Kind::Spawned(info)) => info.id,
        other => panic!("spawn: {other:?}"),
    }
}

fn read_when_there(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reads `conn`'s frames on a thread of their own.
fn frames(conn: Connection) -> std::sync::mpsc::Receiver<Frame> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (mut reader, sender) = conn.split();
    std::thread::spawn(move || {
        let _keep = sender;
        while let Ok(Some(f)) = reader.read_frame() {
            if tx.send(f).is_err() {
                return;
            }
        }
    });
    rx
}

const TOKEN_SCRIPT: &str =
    "printf %s \"$FLEET_AGENT_TOKEN\" > {0}.tmp && mv {0}.tmp {0}; exec sleep 600";

fn spawn_with_token(c: &mut Connection, dir: &Path, name: &str) -> (u64, String) {
    let tok = dir.join(format!("{name}.tok"));
    let id = spawn(c, &TOKEN_SCRIPT.replace("{0}", &tok.display().to_string()));
    (id, read_when_there(&tok))
}

fn ask(question: &str) -> request::Kind {
    request::Kind::Report(ReportStatus {
        state: ReportedState::ReportedWaiting.into(),
        message: "stuck".into(),
        question: Some(question.into()),
        choices: vec!["main".into(), "dev".into()],
    })
}

fn open_question(
    rx: &std::sync::mpsc::Receiver<Frame>,
    wait: Duration,
) -> Vec<fleet_proto::AgentReported> {
    let deadline = Instant::now() + wait;
    let mut out = Vec::new();
    while let Ok(f) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Frame::Event(e) = f
            && let Some(event::Kind::Reported(r)) = e.kind
        {
            out.push(r);
        }
    }
    out
}

fn answer(agent_id: u64, text: &str) -> request::Kind {
    request::Kind::Answer(AnswerQuestion {
        agent_id,
        text: text.into(),
        question_id: None,
    })
}

#[test]
fn a_question_is_seen_by_its_scope_and_only_the_user_answers() {
    let dir = tempfile::tempdir().unwrap();
    let _d = Daemon::start(dir.path());
    let sock = dir.path().join("fleetd.sock");
    let mut user = Connection::unix(&sock, "user").unwrap();
    let (p, p_token) = spawn_with_token(&mut user, dir.path(), "p");
    let (_stranger, s_token) = spawn_with_token(&mut user, dir.path(), "s");
    let mut parent = Connection::unix_as(&sock, "p", Some(&p_token)).unwrap();
    let tok = dir.path().join("c.tok");
    let c = spawn(
        &mut parent,
        &TOKEN_SCRIPT.replace("{0}", &tok.display().to_string()),
    );
    let c_token = read_when_there(&tok);
    let user_events = frames(Connection::unix(&sock, "watch").unwrap());
    let parent_events = frames(Connection::unix_as(&sock, "p2", Some(&p_token)).unwrap());
    let stranger_events = frames(Connection::unix_as(&sock, "s2", Some(&s_token)).unwrap());
    let mut child = Connection::unix_as(&sock, "c", Some(&c_token)).unwrap();

    // The user has no agent to ask for.
    assert!(user.call(ask("ok?")).unwrap_err().is_forbidden());
    child.call(ask("Which branch?")).unwrap();
    for (who, rx) in [("user", &user_events), ("parent", &parent_events)] {
        let got = open_question(rx, Duration::from_millis(1500));
        let q = got
            .iter()
            .filter_map(|r| r.reported.as_ref()?.question.as_ref())
            .next()
            .unwrap_or_else(|| panic!("{who}: no question in {got:?}"));
        assert_eq!(q.text, "Which branch?");
        assert_eq!(q.choices, ["main", "dev"]);
    }
    assert!(open_question(&stranger_events, Duration::from_millis(500)).is_empty());

    // An agent, even the parent or the asker, is refused (exit 3).
    for conn in [&mut parent, &mut child] {
        let err = conn.call(answer(c, "dev")).unwrap_err();
        assert!(err.is_forbidden(), "{err}");
    }
    // The user's answer closes it.
    user.call(answer(c, "dev")).unwrap();
    let got = open_question(&user_events, Duration::from_millis(1500));
    assert!(
        got.iter()
            .any(|r| r.agent_id == c && r.reported.as_ref().is_some_and(|x| x.question.is_none())),
        "{got:?}"
    );
    let err = user.call(answer(c, "again")).unwrap_err();
    assert!(
        !err.is_forbidden() && err.to_string().contains("no open question"),
        "{err}"
    );
    // The list shows no open question.
    match user
        .call(request::Kind::List(ListAgents::default()))
        .unwrap()
    {
        response::Kind::Agents(l) => {
            let a = l.agents.iter().find(|a| a.id == c).unwrap();
            assert!(a.reported.as_ref().unwrap().question.is_none());
        }
        other => panic!("list: {other:?}"),
    }
    let _ = p;
}
