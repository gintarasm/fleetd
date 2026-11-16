//! Kill an agent from its pane header, its sidebar row or ⌘⇧W, after a
//! question that can be turned off (issue #27).

use fleet_gui::core::config::{load_config, save_confirm_kill};
use fleet_gui::core::icons::Icon;
use fleet_gui::core::kill::{AgentButtons, KillFlow, agent_buttons, kill_flow, kill_question};
use fleet_gui::core::menus::{MenuEntry, PaneMenuItem, PaneMenuState, pane_menu};
use fleet_gui::core::status::Subject;

#[test]
fn a_running_agent_offers_stop_an_ended_one_restart_and_remove() {
    assert_eq!(agent_buttons(true, false), AgentButtons::Stop);
    assert_eq!(agent_buttons(true, true), AgentButtons::RestartRemove);
    // An empty pane offers neither.
    assert_eq!(agent_buttons(false, false), AgentButtons::None);
    assert_eq!(agent_buttons(false, true), AgentButtons::None);
}

#[test]
fn the_question_names_the_agent_its_program_and_directory() {
    assert_eq!(
        kill_question(Subject::Agent, 7, Some("claude"), Some("~/fleetd")),
        "Kill agent 7 (claude, ~/fleetd)?"
    );
    assert_eq!(
        kill_question(Subject::Agent, 7, Some("claude"), None),
        "Kill agent 7 (claude)?"
    );
    assert_eq!(
        kill_question(Subject::Agent, 7, Some("claude"), Some("")),
        "Kill agent 7 (claude)?"
    );
    assert_eq!(
        kill_question(Subject::Agent, 9, None, None),
        "Kill agent 9?"
    );
}

/// A shell from New terminal is never called an agent (ADR 0071).
#[test]
fn a_shell_is_asked_about_as_a_shell() {
    assert_eq!(
        kill_question(Subject::Shell, 4, Some("zsh"), Some("~/src")),
        "Kill shell 4 (zsh, ~/src)?"
    );
    assert_eq!(PaneMenuItem::Kill.title(Subject::Shell), "Kill Shell…");
    assert_eq!(PaneMenuItem::Rename.title(Subject::Shell), "Rename Shell…");
    assert_eq!(PaneMenuItem::Restart.title(Subject::Shell), "Restart Shell");
    // Items that are not about what the pane shows keep their title.
    assert_eq!(PaneMenuItem::Close.title(Subject::Shell), "Close Pane");
}

#[test]
fn it_asks_unless_the_user_said_not_to() {
    assert_eq!(kill_flow(true), KillFlow::Ask);
    assert_eq!(kill_flow(false), KillFlow::Kill);
}

#[test]
fn stop_has_its_own_icon() {
    assert_ne!(Icon::Stop, Icon::Close);
    assert_ne!(Icon::Stop.path(), Icon::Close.path());
    assert!(Icon::ALL.contains(&Icon::Stop));
}

#[test]
fn confirm_kill_defaults_on_and_dont_ask_again_saves_it() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_config(dir.path()).unwrap().confirm_kill);
    std::fs::write(
        dir.path().join("config.toml"),
        "# mine\n[gui]\nfont_size = 15 # big\n",
    )
    .unwrap();
    save_confirm_kill(dir.path(), false).unwrap();
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(
        text.contains("# mine") && text.contains("font_size = 15 # big"),
        "{text}"
    );
    let config = load_config(dir.path()).unwrap();
    assert!(!config.confirm_kill);
    assert_eq!(config.font_size, 15.0);
    save_confirm_kill(dir.path(), true).unwrap();
    assert!(load_config(dir.path()).unwrap().confirm_kill);
}

#[test]
fn the_pane_menu_offers_kill_agent_with_cmd_shift_w_for_a_running_agent() {
    let items = |s: PaneMenuState| -> Vec<PaneMenuItem> {
        pane_menu(s)
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item((i, _)) => Some(i),
                MenuEntry::Separator => None,
            })
            .collect()
    };
    let running = PaneMenuState {
        killable: true,
        ..Default::default()
    };
    assert!(items(running).contains(&PaneMenuItem::Kill));
    assert!(!items(PaneMenuState::default()).contains(&PaneMenuItem::Kill));
    let ended = PaneMenuState {
        restartable: true,
        ..Default::default()
    };
    assert!(!items(ended).contains(&PaneMenuItem::Kill));
    assert_eq!(PaneMenuItem::Kill.title(Subject::Agent), "Kill Agent…");
    let key = PaneMenuItem::Kill.shortcut().unwrap();
    assert_eq!((key.key, key.shift), ("w", true));
}
