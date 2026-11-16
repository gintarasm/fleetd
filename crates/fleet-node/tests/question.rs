//! Questions to the user and their answers in the node (ADR 0078, spec §5).

use std::io;
use std::time::{Duration, Instant};

use fleet_node::node::{Node, QuestionIn};
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX};
use fleet_proto::{MessageKind, ReportedState, SpawnAgent};

fn sleeper() -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 600".into()],
        cols: 80,
        rows: 24,
        ..Default::default()
    }
}

fn open_node() -> (tempfile::TempDir, Node) {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(dir.path()).unwrap();
    (dir, node)
}

fn ask(node: &Node, id: u64, q: &str, choices: &[&str]) -> io::Result<()> {
    let choices: Vec<String> = choices.iter().map(|c| (*c).to_owned()).collect();
    node.report_question(
        id,
        ReportedState::ReportedWaiting,
        "stuck",
        Some(QuestionIn {
            text: q,
            choices: &choices,
        }),
    )
}

fn question_of(node: &Node, id: u64) -> Option<fleet_proto::Question> {
    node.list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == id)
        .unwrap()
        .reported
        .and_then(|r| r.question)
}

fn inbox(node: &Node, who: Caller) -> Vec<fleet_proto::Message> {
    node.inbox(who, Some(0), 0).unwrap().0
}

#[test]
fn a_question_goes_to_the_user_and_the_parent_and_is_open_until_answered() {
    let (_d, node) = open_node();
    let p = node.spawn(sleeper()).unwrap().id;
    let c = node.spawn_by(Caller::Agent(p), sleeper()).unwrap().id;
    let mut events = node.report_events();

    ask(&node, c, "Which branch?", &["main", "dev"]).unwrap();
    let q = question_of(&node, c).expect("open question");
    assert_eq!(
        (q.text.as_str(), q.choices.as_slice()),
        (
            "Which branch?",
            &["main".to_string(), "dev".to_string()][..]
        )
    );
    let ev = events.try_recv().unwrap();
    assert_eq!(ev.reported.unwrap().question, Some(q.clone()));

    // The user's mailbox has the question (its id is the question's); the
    // parent has a copy. The reported state is waiting ("needs you").
    let user: Vec<_> = inbox(&node, Caller::User)
        .into_iter()
        .filter(|m| m.kind() == MessageKind::Question)
        .collect();
    assert_eq!(user.len(), 1);
    assert_eq!(user[0].id, q.id);
    assert_eq!(user[0].sender, Some(c));
    assert_eq!(user[0].body, "Which branch?");
    assert_eq!(user[0].choices, ["main", "dev"]);
    let parent: Vec<_> = inbox(&node, Caller::Agent(p))
        .into_iter()
        .filter(|m| m.kind() == MessageKind::Question)
        .collect();
    assert_eq!(parent.len(), 1);
    assert_eq!(parent[0].choices, ["main", "dev"]);

    // The user answers: an answer to the asker and a copy to its parent.
    node.answer(Caller::User, c, Some(q.id), "dev").unwrap();
    assert_eq!(question_of(&node, c), None);
    let ev = events.try_recv().unwrap();
    assert_eq!(ev.reported.as_ref().unwrap().question, None);
    assert_eq!(ev.reported.unwrap().state(), ReportedState::ReportedWorking);
    for who in [c, p] {
        let got: Vec<_> = inbox(&node, Caller::Agent(who))
            .into_iter()
            .filter(|m| m.kind() == MessageKind::Answer)
            .collect();
        assert_eq!(got.len(), 1, "agent {who}");
        assert_eq!((got[0].body.as_str(), got[0].sender), ("dev", None));
    }
    // Closed: a second answer finds none.
    let e = node.answer(Caller::User, c, None, "again").unwrap_err();
    assert!(e.to_string().contains("no open question"), "{e}");
    node.shutdown();
}

#[test]
fn a_root_agents_question_is_one_message_to_the_user() {
    let (_d, node) = open_node();
    let a = node.spawn(sleeper()).unwrap().id;
    ask(&node, a, "ok?", &[]).unwrap();
    let qs: Vec<_> = inbox(&node, Caller::User)
        .into_iter()
        .filter(|m| m.kind() == MessageKind::Question)
        .collect();
    assert_eq!(qs.len(), 1);
    node.answer(Caller::User, a, None, "yes").unwrap();
    // No parent: nobody else gets a copy.
    assert_eq!(
        inbox(&node, Caller::User)
            .iter()
            .filter(|m| m.kind() == MessageKind::Answer)
            .count(),
        0
    );
    node.shutdown();
}

#[test]
fn a_later_report_closes_the_question() {
    let (_d, node) = open_node();
    let a = node.spawn(sleeper()).unwrap().id;
    ask(&node, a, "ok?", &["y"]).unwrap();
    node.report(a, ReportedState::ReportedWorking, "carrying on")
        .unwrap();
    assert_eq!(question_of(&node, a), None);
    let e = node.answer(Caller::User, a, None, "y").unwrap_err();
    assert!(e.to_string().contains("no open question"), "{e}");
    // A new question is a new one; an answer naming the old id is stale.
    ask(&node, a, "again?", &[]).unwrap();
    let q = question_of(&node, a).unwrap();
    let e = node
        .answer(Caller::User, a, Some(q.id - 1), "late")
        .unwrap_err();
    assert!(e.to_string().contains("no open question"), "{e}");
    assert!(question_of(&node, a).is_some());
    node.shutdown();
}

#[test]
fn only_the_user_answers() {
    let (_d, node) = open_node();
    let p = node.spawn(sleeper()).unwrap().id;
    let c = node.spawn_by(Caller::Agent(p), sleeper()).unwrap().id;
    ask(&node, c, "ok?", &[]).unwrap();
    for who in [p, c] {
        let e = node.answer(Caller::Agent(who), c, None, "yes").unwrap_err();
        assert!(e.to_string().starts_with(FORBIDDEN_PREFIX), "{e}");
    }
    assert!(question_of(&node, c).is_some());
    node.shutdown();
}

#[test]
fn bad_questions_and_answers_are_refused() {
    let (_d, node) = open_node();
    let a = node.spawn(sleeper()).unwrap().id;
    let seven: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g"];
    let long_choice = "x".repeat(61);
    let long_q = "q".repeat(501);
    for (q, choices) in [
        ("", vec![]),
        ("two\nlines", vec![]),
        (long_q.as_str(), vec![]),
        ("ok?", seven),
        ("ok?", vec![long_choice.as_str()]),
        ("ok?", vec![""]),
        ("ok?", vec!["a\tb"]),
    ] {
        assert!(ask(&node, a, q, &choices).is_err(), "{q:?} {choices:?}");
    }
    // Only with `waiting`, and choices need a question.
    let e = node.report_question(
        a,
        ReportedState::ReportedDone,
        "",
        Some(QuestionIn {
            text: "ok?",
            choices: &[],
        }),
    );
    assert!(e.is_err());
    assert_eq!(question_of(&node, a), None);
    // The limits themselves are fine; 60 characters, not bytes.
    let six = ["a", "b", "c", "d", "e", "f"];
    ask(&node, a, &"q".repeat(500), &six).unwrap();
    ask(&node, a, "ok?", &[&"é".repeat(60)]).unwrap();
    // An empty answer, an over-long one, an unknown agent.
    assert!(node.answer(Caller::User, a, None, "").is_err());
    assert!(
        node.answer(Caller::User, a, None, &"x".repeat(4097))
            .is_err()
    );
    assert!(node.answer(Caller::User, 999, None, "x").is_err());
    node.answer(Caller::User, a, None, "fine").unwrap();
    node.shutdown();
}

#[test]
fn an_exited_agents_question_cannot_be_answered() {
    let (_d, node) = open_node();
    let a = node.spawn(sleeper()).unwrap().id;
    ask(&node, a, "ok?", &[]).unwrap();
    node.kill(a).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.exit_code(a).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(node.answer(Caller::User, a, None, "late").is_err());
    node.shutdown();
}
