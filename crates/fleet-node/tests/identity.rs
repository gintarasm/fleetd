//! Agent identity, parents, scope and quotas in the node (ADR 0072, spec §1).

use fleet_node::node::Node;
use fleet_node::scope::{Caller, FORBIDDEN_PREFIX, PARENT_LABEL, Quotas};
use fleet_node::store::Labels;
use fleet_proto::SpawnAgent;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

fn sh(script: &str, l: &[(&str, &str)]) -> SpawnAgent {
    SpawnAgent {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cols: 80,
        rows: 24,
        labels: labels(l),
        ..Default::default()
    }
}

/// An agent that writes its pid (its session id) and token to files in
/// `dir`, then sleeps.
fn recorder(dir: &Path, tag: &str, l: &[(&str, &str)]) -> SpawnAgent {
    let d = dir.display();
    sh(
        &format!(
            "echo $$ > {d}/{tag}.pid.tmp && mv {d}/{tag}.pid.tmp {d}/{tag}.pid; \
             printf %s \"$FLEET_AGENT_TOKEN\" > {d}/{tag}.tok.tmp && mv {d}/{tag}.tok.tmp {d}/{tag}.tok; \
             exec sleep 600"
        ),
        l,
    )
}

fn read_when_there(path: PathBuf) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s.trim().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The pid and token an agent spawned from [`recorder`] wrote.
fn recorded(dir: &Path, tag: &str) -> (u32, String) {
    let pid = read_when_there(dir.join(format!("{tag}.pid")));
    let token = read_when_there(dir.join(format!("{tag}.tok")));
    (pid.parse().unwrap(), token)
}

fn wait_exit(node: &Node, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.exit_code(id).is_none() {
        assert!(Instant::now() < deadline, "agent {id} did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sleeper() -> SpawnAgent {
    sh("exec sleep 600", &[])
}

fn is_forbidden(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied && e.to_string().starts_with(FORBIDDEN_PREFIX)
}

#[test]
fn the_session_and_the_token_identify_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let a = node.spawn(recorder(dir.path(), "a", &[])).unwrap().id;
    let b = node.spawn(recorder(dir.path(), "b", &[])).unwrap().id;
    let (pid_a, tok_a) = recorded(dir.path(), "a");
    let (_, tok_b) = recorded(dir.path(), "b");
    assert_eq!(tok_a.len(), 64);
    assert_ne!(tok_a, tok_b);

    // Session alone, token alone, both agreeing.
    assert_eq!(node.resolve_caller(Some(pid_a), None), Ok(Caller::Agent(a)));
    assert_eq!(
        node.resolve_caller(None, Some(&tok_a)),
        Ok(Caller::Agent(a))
    );
    assert_eq!(
        node.resolve_caller(Some(pid_a), Some(&tok_a)),
        Ok(Caller::Agent(a))
    );
    assert_eq!(
        node.resolve_caller(None, Some(&tok_b)),
        Ok(Caller::Agent(b))
    );
    // B's token in A's session is refused.
    let e = node.resolve_caller(Some(pid_a), Some(&tok_b)).unwrap_err();
    assert!(
        e.contains(&format!("agent {a}")) && e.contains(&format!("agent {b}")),
        "{e}"
    );
    // A session no agent leads, and no token: the user.
    assert_eq!(
        node.resolve_caller(Some(std::process::id()), None),
        Ok(Caller::User)
    );
    assert_eq!(node.resolve_caller(None, None), Ok(Caller::User));
    // A made-up token is refused, not taken for the user.
    assert!(node.resolve_caller(None, Some("00")).is_err());

    // Exit revokes the token, and the session no longer names A.
    node.kill(a).unwrap();
    wait_exit(&node, a);
    assert!(node.resolve_caller(None, Some(&tok_a)).is_err());
    assert_eq!(node.resolve_caller(Some(pid_a), None), Ok(Caller::User));
    node.shutdown();
}

#[test]
fn a_request_cannot_set_the_token_and_a_restart_gets_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let mut req = recorder(dir.path(), "a", &[]);
    req.env.insert("FLEET_AGENT_TOKEN".into(), "forged".into());
    let a = node.spawn(req).unwrap().id;
    let (_, tok) = recorded(dir.path(), "a");
    assert_ne!(tok, "forged");
    node.kill(a).unwrap();
    wait_exit(&node, a);
    for f in ["a.pid", "a.tok"] {
        std::fs::remove_file(dir.path().join(f)).unwrap();
    }
    let out = node.restart(a, 80, 24, None).unwrap();
    let (_, tok2) = recorded(dir.path(), "a");
    assert_ne!(tok2, tok);
    assert_eq!(
        node.resolve_caller(None, Some(&tok2)),
        Ok(Caller::Agent(out.info.id))
    );
    node.shutdown();
}

#[test]
fn a_root_shell_is_the_user_but_an_agents_shell_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    node.spawn(recorder(dir.path(), "s", &[("kind", "shell")]))
        .unwrap();
    let (pid, tok) = recorded(dir.path(), "s");
    assert_eq!(node.resolve_caller(Some(pid), None), Ok(Caller::User));
    assert_eq!(node.resolve_caller(None, Some(&tok)), Ok(Caller::User));

    let a = node.spawn(sleeper()).unwrap().id;
    let child = node
        .spawn_by(
            Caller::Agent(a),
            recorder(dir.path(), "c", &[("kind", "shell")]),
        )
        .unwrap()
        .id;
    let (pid, _) = recorded(dir.path(), "c");
    assert_eq!(
        node.resolve_caller(Some(pid), None),
        Ok(Caller::Agent(child))
    );
    node.shutdown();
}

#[test]
fn an_agents_spawn_is_its_child_with_narrowed_labels() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let mut req = sleeper();
    req.labels = labels(&[("project", "fleetd"), ("name", "boss")]);
    let p = node.spawn(req).unwrap().id;
    assert!(!node.list().unwrap()[0].labels.contains_key(PARENT_LABEL));

    let mut req = sleeper();
    req.labels = labels(&[("role", "reviewer"), ("name", "kid")]);
    let c = node.spawn_by(Caller::Agent(p), req).unwrap();
    assert_eq!(
        c.labels,
        labels(&[
            ("project", "fleetd"),
            ("role", "reviewer"),
            ("name", "kid"),
            (PARENT_LABEL, &p.to_string()),
        ])
    );

    // A changed parent key is refused; so is a reserved one.
    let mut req = sleeper();
    req.labels = labels(&[("project", "elsewhere")]);
    let e = node.spawn_by(Caller::Agent(p), req).unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    let mut req = sleeper();
    req.labels = labels(&[(PARENT_LABEL, "1")]);
    assert!(node.spawn_by(Caller::Agent(c.id), req).is_err());

    // Agents see their descendants only; the user sees all.
    let g = node.spawn_by(Caller::Agent(c.id), sleeper()).unwrap().id;
    let other = node.spawn(sleeper()).unwrap().id;
    let seen = |caller| -> Vec<u64> {
        let mut ids: Vec<u64> = node
            .list_for(caller, &Labels::new())
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(seen(Caller::Agent(p)), vec![c.id, g]);
    assert_eq!(seen(Caller::Agent(c.id)), vec![g]);
    assert!(seen(Caller::Agent(g)).is_empty());
    assert_eq!(seen(Caller::User), vec![p, c.id, g, other]);
    node.shutdown();
}

#[test]
fn quotas_refuse_the_fourth_level_and_the_ninth_child_but_never_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state"))
        .unwrap()
        .with_quotas(Quotas::default());
    let mut refused = node.refused_events();
    let root = node.spawn(sleeper()).unwrap().id;
    let mut at = root;
    for _ in 0..3 {
        at = node.spawn_by(Caller::Agent(at), sleeper()).unwrap().id;
    }
    let e = node.spawn_by(Caller::Agent(at), sleeper()).unwrap_err();
    assert!(
        is_forbidden(&e) && e.to_string().contains("max_depth"),
        "{e}"
    );
    let ev = refused.try_recv().unwrap();
    assert_eq!(ev.parent, at);
    assert!(ev.reason.contains("max_depth"), "{}", ev.reason);

    // The root's 8 running children are its limit (it has one already).
    for _ in 0..7 {
        node.spawn_by(Caller::Agent(root), sleeper()).unwrap();
    }
    let e = node.spawn_by(Caller::Agent(root), sleeper()).unwrap_err();
    assert!(
        is_forbidden(&e) && e.to_string().contains("max_children"),
        "{e}"
    );
    // The user's spawns are never refused.
    node.spawn(sleeper()).unwrap();
    node.shutdown();
}

#[test]
fn quotas_refuse_the_31st_agent_but_never_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state")).unwrap();
    let root = node.spawn(sleeper()).unwrap().id;
    for _ in 0..29 {
        node.spawn(sleeper()).unwrap();
    }
    let e = node.spawn_by(Caller::Agent(root), sleeper()).unwrap_err();
    assert!(
        is_forbidden(&e) && e.to_string().contains("max_agents"),
        "{e}"
    );
    node.spawn(sleeper()).unwrap();
    node.shutdown();
}

#[test]
fn an_agents_restart_of_its_child_keeps_the_parent_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::open(&dir.path().join("state"))
        .unwrap()
        .with_quotas(Quotas {
            max_children: 1,
            ..Quotas::default()
        });
    let p = node.spawn(sleeper()).unwrap().id;
    let c = node
        .spawn_by(Caller::Agent(p), sh("exit 0", &[]))
        .unwrap()
        .id;
    wait_exit(&node, c);
    let out = node.restart_by(Caller::Agent(p), c, 80, 24, None).unwrap();
    assert_eq!(out.info.labels.get(PARENT_LABEL), Some(&p.to_string()));
    // Now at its one running child: another spawn, or restart, is refused.
    let e = node.spawn_by(Caller::Agent(p), sleeper()).unwrap_err();
    assert!(is_forbidden(&e), "{e}");
    node.shutdown();
}
