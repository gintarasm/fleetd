//! The New agent sheet: a directory (typed, picked from the recent ones,
//! or chosen with the native folder picker for a local daemon) and a
//! command, prefilled from `[gui] default_command`.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::controls::{
    ButtonKind, button, error_text, field_label, list_row, list_well, sheet_card, sheet_title,
};
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use crate::core::session::AgentRow;
use crate::core::style::{space, text};
use crate::core::target::Target;

/// What a spawn needs from the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
}

pub enum SheetEvent {
    Spawn(SpawnRequest),
    Cancel,
}

/// Splits a command line with shell quoting rules into program and args.
pub fn parse_command(line: &str) -> Result<(String, Vec<String>), String> {
    let mut words = shell_words::split(line)
        .map_err(|e| format!("cannot parse the command: {e}"))?
        .into_iter();
    let program = words.next().ok_or("enter a command")?;
    Ok((program, words.collect()))
}

/// The sheet's directory list: the recent directories for the target,
/// then the other directories of its agents, newest agent first.
pub fn dir_choices(recent: &[String], agents: &[AgentRow]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in recent {
        if !d.is_empty() && !out.contains(d) {
            out.push(d.clone());
        }
    }
    let mut rows: Vec<&AgentRow> = agents.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.info.created_unix_ms));
    for r in rows {
        if !r.info.cwd.is_empty() && !out.contains(&r.info.cwd) {
            out.push(r.info.cwd.clone());
        }
    }
    out
}

/// The note under the directory field: a remote daemon's paths are the
/// remote host's, passed as typed.
pub fn dir_hint(target: &Target) -> Option<String> {
    match target {
        Target::Local => None,
        Target::Ssh { dest } => {
            let host = dest.rsplit_once('@').map_or(dest.as_str(), |(_, h)| h);
            Some(format!("paths on {host}; ~ is not expanded"))
        }
    }
}

pub struct NewAgentSheet {
    pub(crate) dir: Entity<TextField>,
    pub(crate) command: Entity<TextField>,
    pub(crate) dirs: Vec<String>,
    /// The native folder picker is offered (local daemon only).
    pub(crate) can_choose: bool,
    /// [`dir_hint`] for the target.
    pub(crate) hint: Option<String>,
    pub(crate) error: Option<String>,
}

impl EventEmitter<SheetEvent> for NewAgentSheet {}

impl NewAgentSheet {
    pub fn new(
        default_command: &[String],
        dirs: Vec<String>,
        target: &Target,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = dirs.first().cloned().unwrap_or_default();
        let command = shell_words::join(default_command);
        NewAgentSheet {
            dir: cx.new(|cx| TextField::new(first, "Directory on the fleetd host", cx)),
            command: cx.new(|cx| TextField::new(command, "Command", cx)),
            dirs,
            can_choose: *target == Target::Local,
            hint: dir_hint(target),
            error: None,
        }
    }

    /// Focus goes to the command field.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.command.read(cx).focus_handle());
    }

    /// Validates the fields and asks for the spawn.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let cwd = self.dir.read(cx).text().trim().to_owned();
        if cwd.is_empty() {
            self.error = Some("enter a directory".into());
            cx.notify();
            return;
        }
        match parse_command(self.command.read(cx).text()) {
            Ok((program, args)) => {
                self.error = None;
                cx.emit(SheetEvent::Spawn(SpawnRequest { program, args, cwd }));
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// Shows why the spawn did not start.
    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(SheetEvent::Cancel);
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        let dir = self.dir.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = dir.update(cx, |f, cx| {
                    f.set_text(path.to_string_lossy().into_owned(), cx);
                });
            }
        })
        .detach();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" => {
                self.submit(cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.cancel(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for NewAgentSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mut dir_row = div()
            .flex()
            .flex_row()
            .gap(px(space::SM))
            .child(div().flex_1().child(self.dir.clone()));
        if self.can_choose {
            dir_row = dir_row.child(
                button("choose", "Choose…", &theme, ButtonKind::Secondary, true)
                    .on_click(cx.listener(|this, _, _, cx| this.choose(cx))),
            );
        }
        let recent = (!self.dirs.is_empty()).then(|| {
            list_well("recent-dirs", 140.0, &theme).children(self.dirs.iter().enumerate().map(
                |(i, d)| {
                    let path = d.clone();
                    list_row(("dir", i), d.clone(), &theme, true).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let path = path.clone();
                            this.dir.update(cx, |f, cx| f.set_text(path, cx));
                        },
                    ))
                },
            ))
        });
        sheet_card("new-agent-sheet", 520.0, &theme)
            .debug_selector(|| "new-agent-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("New agent"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Directory", &theme))
                    .child(dir_row)
                    .children(self.hint.clone().map(|h| {
                        div()
                            .debug_selector(|| "dir-hint".to_owned())
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.tertiary)
                            .child(SharedString::from(h))
                    }))
                    .children(recent),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Command", &theme))
                    .child(self.command.clone()),
            )
            .children(self.error.clone().map(|e| error_text(e, &theme)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(space::SM))
                    .pt(px(space::XS))
                    .child(
                        button("cancel", "Cancel", &theme, ButtonKind::Secondary, true)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button("spawn", "Spawn", &theme, ButtonKind::Primary, true)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{dir_choices, dir_hint, parse_command};
    use crate::core::session::AgentRow;
    use crate::core::target::Target;
    use fleet_proto::AgentInfo;

    fn row(cwd: &str, created: i64) -> AgentRow {
        AgentRow {
            info: AgentInfo {
                cwd: cwd.into(),
                created_unix_ms: created,
                ..Default::default()
            },
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn remote_directories_carry_a_hint() {
        assert_eq!(dir_hint(&Target::Local), None);
        let remote = Target::Ssh {
            dest: "me@box".into(),
        };
        assert_eq!(
            dir_hint(&remote).as_deref(),
            Some("paths on box; ~ is not expanded")
        );
    }

    #[test]
    fn command_uses_shell_quoting() {
        assert_eq!(
            parse_command("sh -c 'echo \"hi there\"'"),
            Ok((
                "sh".to_owned(),
                vec!["-c".to_owned(), "echo \"hi there\"".to_owned()]
            ))
        );
        assert_eq!(parse_command("claude"), Ok(("claude".to_owned(), vec![])));
        assert!(parse_command("   ").is_err());
        assert!(parse_command("echo 'open").is_err());
    }

    #[test]
    fn dirs_are_recent_then_agent_cwds_newest_first_distinct() {
        let recent = vec!["/a".to_owned(), "/b".to_owned()];
        let agents = [row("/old", 1), row("/b", 5), row("/new", 9), row("/new", 3)];
        assert_eq!(dir_choices(&recent, &agents), ["/a", "/b", "/new", "/old"]);
    }
}
