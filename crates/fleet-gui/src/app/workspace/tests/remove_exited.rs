use super::*;

#[gpui::test]
fn remove_exited_asks_with_the_count_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    // Nothing ended: nothing is asked or sent.
    agents(&mut s, &fake, vec![row(7, "/w", 1)]);
    s.cx.dispatch_action(RemoveExited);
    assert!(!s.cx.has_pending_prompt());
    assert!(!fake.calls().contains(&Call::ClearExited));
    agents(
        &mut s,
        &fake,
        vec![
            row(7, "/w", 1),
            ended(8, "/w", 2, AgentState::Exited),
            ended(9, "/w", 3, AgentState::Lost),
        ],
    );
    s.cx.dispatch_action(RemoveExited);
    assert!(s.cx.has_pending_prompt());
    s.cx.simulate_prompt_answer("Cancel");
    s.cx.run_until_parked();
    assert!(!fake.calls().contains(&Call::ClearExited));
    s.cx.dispatch_action(RemoveExited);
    s.cx.simulate_prompt_answer("Remove");
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
    // Never a per-id kill or forget of the running agent.
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::Forget(_))));
}

#[gpui::test]
fn remove_exited_from_the_palette_asks_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(&mut s, &fake, vec![ended(8, "/w", 2, AgentState::Exited)]);
    s.cx.simulate_keystrokes("cmd-p");
    s.cx.simulate_input("remove exited");
    s.cx.simulate_keystrokes("enter");
    assert!(s.cx.has_pending_prompt());
    s.cx.simulate_prompt_answer("Remove");
    s.cx.run_until_parked();
    assert_eq!(fake.calls().last(), Some(&Call::ClearExited));
}

fn child_of(mut r: AgentRow, parent: u64) -> AgentRow {
    r.info
        .labels
        .insert("fleet.parent".into(), parent.to_string());
    r
}

#[gpui::test]
fn remove_exited_keeps_an_ended_parent_of_a_running_agent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            // Ended, but its child runs: stays.
            ended(1, "/w", 1, AgentState::Exited),
            child_of(row(2, "/w", 2), 1),
            // An ended branch: goes, children and all.
            ended(3, "/w", 3, AgentState::Exited),
            child_of(ended(4, "/w", 4, AgentState::Lost), 3),
        ],
    );
    s.cx.dispatch_action(RemoveExited);
    assert!(s.cx.has_pending_prompt());
    s.cx.simulate_prompt_answer("Remove");
    s.cx.run_until_parked();
    let Some(Call::Forget(mut ids)) = fake.calls().last().cloned() else {
        panic!("no forget: {:?}", fake.calls());
    };
    ids.sort_unstable();
    assert_eq!(ids, [3, 4]);
    // Only a parent of a running agent is ended: nothing to remove.
    agents(
        &mut s,
        &fake,
        vec![
            ended(1, "/w", 1, AgentState::Exited),
            child_of(row(2, "/w", 2), 1),
        ],
    );
    let before = fake.calls().len();
    s.cx.dispatch_action(RemoveExited);
    assert!(!s.cx.has_pending_prompt());
    assert_eq!(fake.calls().len(), before);
}

#[gpui::test]
fn children_are_drawn_under_their_parent_and_a_big_family_starts_collapsed(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    let mut rows = vec![row(1, "/w", 1)];
    rows.extend((2..=4).map(|i| child_of(row(i, "/w", i64::try_from(i).unwrap()), 1)));
    agents(&mut s, &fake, rows.clone());
    assert_eq!(tabs_drawn(&mut s), [1, 2, 3, 4]);
    rows.push(child_of(row(5, "/w", 5), 1));
    agents(&mut s, &fake, rows);
    assert_eq!(tabs_drawn(&mut s), [1]);
    click(&mut s, "disclose-1");
    assert_eq!(tabs_drawn(&mut s), [1, 2, 3, 4, 5]);
    click(&mut s, "disclose-1");
    assert_eq!(tabs_drawn(&mut s), [1]);
}
