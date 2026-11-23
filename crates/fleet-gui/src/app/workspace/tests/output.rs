use super::*;

/// Agent 5 in pane 1, running, with `text` on screen; `read_output`
/// gives its history.
fn output_setup<'a>(cx: &'a mut TestAppContext, fake: &Arc<Fake>, dir: &Path) -> Setup<'a> {
    let mut s = setup(cx, fake, dir);
    let mut named = row(5, "/w", 1);
    named.info.labels.insert("name".into(), "otter".into());
    agents(&mut s, fake, vec![named]);
    click(&mut s, "tab-5");
    attach_model(fake, PaneId(1), 5);
    fake.model(PaneId(1))
        .lock()
        .unwrap()
        .on_output(b"\x1b[31merror\x1b[0m: boom   \r\n");
    fake.outputs
        .lock()
        .unwrap()
        .insert(5, Ok("old line\nerror: boom\n".into()));
    s.ui.try_send(UiEvent::PaneChanged(PaneId(1))).unwrap();
    s.cx.run_until_parked();
    s
}

fn pick_pane(s: &mut Setup<'_>, item: PaneMenuItem) {
    s.ws.update_in(s.cx, |ws, window, cx| {
        let epoch = ws.epoch;
        ws.pane_menu_pick(PaneId(1), item, epoch, window, cx);
    });
    s.cx.run_until_parked();
}

#[gpui::test]
fn save_output_writes_the_history_where_the_panel_says(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = output_setup(cx, &fake, dir.path());
    let file = dir.path().join("out.txt");
    // ⇧⌘S asks where, for the focused pane's agent.
    s.cx.simulate_keystrokes("cmd-shift-s");
    assert!(s.cx.did_prompt_for_new_path());
    let to = file.clone();
    s.cx.simulate_new_path_selection(|_| Some(to));
    s.cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "old line\nerror: boom\n"
    );
    let notice = s.app.read_with(s.cx, |a, _| a.notice().map(str::to_owned));
    assert!(notice.unwrap().starts_with("Saved output to "));

    // Cancelled: nothing written.
    std::fs::remove_file(&file).unwrap();
    pick_pane(&mut s, PaneMenuItem::SaveOutput);
    s.cx.simulate_new_path_selection(|_| None);
    s.cx.run_until_parked();
    assert!(!file.exists());

    // Exited agents too, from the row menu; a failed read is shown.
    fake.model(PaneId(1)).lock().unwrap().on_exited(1);
    agents(&mut s, &fake, vec![ended(5, "/w", 1, AgentState::Exited)]);
    pick_row(&mut s, 5, RowMenuItem::SaveOutput);
    let to = file.clone();
    s.cx.simulate_new_path_selection(|_| Some(to));
    s.cx.run_until_parked();
    assert!(file.exists());
    fake.outputs.lock().unwrap().clear();
    pick_row(&mut s, 5, RowMenuItem::SaveOutput);
    let to = dir.path().join("other.txt");
    s.cx.simulate_new_path_selection(|_| Some(to));
    s.cx.run_until_parked();
    let error = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("Cannot save the output of sh: no such agent")
    );
    assert!(!dir.path().join("other.txt").exists());
}

#[gpui::test]
fn open_output_writes_a_temp_file_and_opens_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = output_setup(cx, &fake, dir.path());
    let opened: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let seen = opened.clone();
    s.cx.update(|_, cx| {
        cx.set_global(crate::app::output::Opener(Arc::new(move |p: &Path| {
            seen.lock().unwrap().push(p.to_owned());
            Ok(())
        })))
    });
    pick_pane(&mut s, PaneMenuItem::OpenOutput);
    let opened = opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].ends_with(format!(
        "fleet-output/otter-{}.txt",
        crate::core::output::today()
    )));
    assert_eq!(
        std::fs::read_to_string(&opened[0]).unwrap(),
        "old line\nerror: boom\n"
    );
}

#[gpui::test]
fn copy_last_output_copies_the_screen_as_text(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = output_setup(cx, &fake, dir.path());
    s.cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    pick_pane(&mut s, PaneMenuItem::CopyLastOutput);
    let clip = s.cx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(clip.as_deref(), Some("error: boom"));
}

#[gpui::test]
fn save_output_first_removes_only_what_was_saved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let mut s = setup(cx, &fake, dir.path());
    agents(
        &mut s,
        &fake,
        vec![
            ended(8, "/w", 2, AgentState::Exited),
            ended(9, "/w", 3, AgentState::Lost),
            ended(10, "/w", 4, AgentState::Exited),
        ],
    );
    fake.outputs.lock().unwrap().insert(8, Ok("eight\n".into()));
    fake.outputs.lock().unwrap().insert(10, Ok("ten\n".into()));
    // The question offers it.
    s.cx.dispatch_action(RemoveExited);
    let (_, detail) = s.cx.pending_prompt().unwrap();
    assert!(detail.contains("deleted"));
    s.cx.simulate_prompt_answer("Cancel");
    s.cx.run_until_parked();

    let out = dir.path().join("saved");
    std::fs::create_dir(&out).unwrap();
    let ids = s.app.read_with(s.cx, |a, _| a.ended_ids());
    assert_eq!(ids, [8, 9, 10]);
    let app = s.app.clone();
    let to = out.clone();
    s.cx.update(|_, cx| crate::app::output::save_all_then_remove(app, ids, to, cx));
    s.cx.run_until_parked();
    // All three are "sh": numbered, never overwritten.
    let date = crate::core::output::today();
    assert_eq!(
        std::fs::read_to_string(out.join(format!("sh-{date}.txt"))).unwrap(),
        "eight\n"
    );
    assert_eq!(
        std::fs::read_to_string(out.join(format!("sh-{date}-2.txt"))).unwrap(),
        "ten\n"
    );
    assert_eq!(fake.calls().last(), Some(&Call::Forget(vec![8, 10])));
    let error = s.app.read_with(s.cx, |a, _| a.error().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("Cannot save the output of sh: no such agent. It was not removed.")
    );
}
