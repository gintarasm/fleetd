//! The New agent sheet: the command presets as one-click choices (ADR
//! 0084), a directory (typed, picked from the recent ones, or chosen with
//! the native folder picker for a local daemon), a command, prefilled
//! from `[gui] default_command` and offered from the recent ones, an
//! optional first prompt (the command's last argument), a name
//! (generated from the command and prompt until edited, ADR 0052) and
//! optional labels.

use fleet_client::names::NAME_LABEL;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use super::controls::{
    ButtonKind, button, error_text, field_label, list_row, list_well, move_focus,
    return_is_not_a_click, sheet_card, sheet_title, tab_stop,
};
use super::text_field::TextField;
use super::theme::{Typography, current as theme};
use crate::core::presets::{Prefill, Preset, append_prompt, name_source};
use crate::core::session::AgentRow;
use crate::core::style::{space, text};
use crate::core::target::Target;

use crate::core::spawn::{NameDraft, parse_label_list};
pub use crate::core::spawn::{SpawnRequest, parse_command};

pub enum SheetEvent {
    /// Spawn `req`; `command` is the Command field as typed (without the
    /// prompt), for the recent commands.
    Spawn {
        req: SpawnRequest,
        command: String,
    },
    Cancel,
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
    /// The first prompt: appended as the command's last argument.
    pub(crate) prompt: Entity<TextField>,
    pub(crate) name: Entity<TextField>,
    pub(crate) labels: Entity<TextField>,
    /// The Name field's generated text, until the user edits it.
    pub(crate) draft: NameDraft,
    pub(crate) dirs: Vec<String>,
    /// The recent commands, most recent first.
    pub(crate) commands: Vec<String>,
    /// The one-click choices at the top.
    pub(crate) presets: Vec<Preset>,
    /// The native folder picker is offered (local daemon only).
    pub(crate) can_choose: bool,
    /// [`dir_hint`] for the target.
    pub(crate) hint: Option<String>,
    pub(crate) error: Option<String>,
    _observers: [Subscription; 4],
}

/// What a [`NewAgentSheet`] offers and starts with.
pub struct SheetSetup {
    /// `[gui] default_command`, unless `prefill` names a command.
    pub default_command: Vec<String>,
    /// The directory list; the first is the Directory field's text unless
    /// `prefill` names one.
    pub dirs: Vec<String>,
    pub recent_commands: Vec<String>,
    pub presets: Vec<Preset>,
    pub prefill: Prefill,
}

impl EventEmitter<SheetEvent> for NewAgentSheet {}

impl NewAgentSheet {
    /// `taken`: the live agents' names, which the generated name avoids.
    pub fn new(
        setup: SheetSetup,
        target: &Target,
        taken: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let SheetSetup {
            default_command,
            dirs,
            recent_commands,
            presets,
            prefill,
        } = setup;
        let first = prefill
            .dir
            .clone()
            .unwrap_or_else(|| dirs.first().cloned().unwrap_or_default());
        let command = prefill
            .command
            .clone()
            .unwrap_or_else(|| shell_words::join(&default_command));
        let draft = NameDraft::new(&command, &first, taken);
        let generated = draft.generated().to_owned();
        let dir = cx.new(|cx| TextField::new(first, "Directory on the fleetd host", cx).tab_stop());
        let command = cx.new(|cx| TextField::new(command, "Command", cx).tab_stop());
        let prompt = cx.new(|cx| {
            TextField::new(
                String::new(),
                "Optional: the agent's first task, as its last argument",
                cx,
            )
            .tab_stop()
        });
        let name = cx.new(|cx| TextField::new(generated, "Name", cx).tab_stop());
        let labels = cx.new(|cx| {
            TextField::new(prefill.labels.clone(), "key=value, key=value", cx).tab_stop()
        });
        // A field notifies on every edit (and caret blink): each observer
        // only acts on a text that changed.
        let observers = [
            cx.observe(&command, |this, _, cx| this.inputs_changed(cx)),
            cx.observe(&dir, |this, _, cx| this.inputs_changed(cx)),
            cx.observe(&prompt, |this, _, cx| this.inputs_changed(cx)),
            cx.observe(&name, |this, name, cx| {
                this.draft.field_changed(name.read(cx).text());
            }),
        ];
        NewAgentSheet {
            dir,
            command,
            prompt,
            name,
            labels,
            draft,
            dirs,
            commands: recent_commands,
            presets,
            can_choose: *target == Target::Local,
            hint: dir_hint(target),
            error: None,
            _observers: observers,
        }
    }

    /// The command, prompt or directory changed: the Name field follows,
    /// until the user edits it.
    fn inputs_changed(&mut self, cx: &mut Context<Self>) {
        let command = name_source(self.command.read(cx).text(), self.prompt.read(cx).text());
        let cwd = self.dir.read(cx).text().trim().to_owned();
        if let Some(name) = self.draft.inputs_changed(&command, &cwd) {
            self.name.update(cx, |f, cx| f.set_text(name, cx));
        }
    }

    /// Focus goes to the command field.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(self.command.read(cx).focus_handle());
    }

    /// A preset's one click: its command and labels fill the fields, and
    /// the prompt gets the keyboard (Return spawns).
    pub fn apply_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.presets.get(index) else {
            return;
        };
        let fill = Prefill::preset(p);
        if let Some(command) = fill.command {
            self.command.update(cx, |f, cx| f.set_text(command, cx));
        }
        self.labels.update(cx, |f, cx| f.set_text(fill.labels, cx));
        window.focus(self.prompt.read(cx).focus_handle());
        cx.notify();
    }

    /// Validates the fields and asks for the spawn.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let cwd = self.dir.read(cx).text().trim().to_owned();
        if cwd.is_empty() {
            self.error = Some("enter a directory".into());
            cx.notify();
            return;
        }
        let command = self.command.read(cx).text().to_owned();
        let request = || -> Result<SpawnRequest, String> {
            let (program, mut args) = parse_command(&command)?;
            append_prompt(&mut args, self.prompt.read(cx).text());
            let mut labels = parse_label_list(self.labels.read(cx).text())?;
            let name = self.draft.name(self.name.read(cx).text())?;
            labels.insert(NAME_LABEL.to_owned(), name);
            Ok(SpawnRequest {
                program,
                args,
                cwd: cwd.clone(),
                labels,
            })
        };
        match request() {
            Ok(req) => {
                self.error = None;
                cx.emit(SheetEvent::Spawn { req, command });
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

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if move_focus(ev, window) {
            cx.stop_propagation();
            return;
        }
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
                tab_stop(
                    button("choose", "Choose…", &theme, ButtonKind::Secondary, true),
                    &theme,
                )
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
        let presets = (!self.presets.is_empty()).then(|| {
            div()
                .debug_selector(|| "presets".to_owned())
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::XS))
                .children(self.presets.iter().enumerate().map(|(i, p)| {
                    tab_stop(
                        button(
                            ("preset", i),
                            p.name.clone(),
                            &theme,
                            ButtonKind::Secondary,
                            true,
                        ),
                        &theme,
                    )
                    .debug_selector(move || format!("preset-{i}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.apply_preset(i, window, cx);
                    }))
                }))
        });
        let recent_commands = (!self.commands.is_empty()).then(|| {
            list_well("recent-commands", 96.0, &theme).children(
                self.commands.iter().enumerate().map(|(i, c)| {
                    let line = c.clone();
                    list_row(("command", i), c.clone(), &theme, true).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let line = line.clone();
                            this.command.update(cx, |f, cx| f.set_text(line, cx));
                        },
                    ))
                }),
            )
        });
        return_is_not_a_click(sheet_card("new-agent-sheet", 520.0, &theme))
            .debug_selector(|| "new-agent-sheet".to_owned())
            .on_key_down(cx.listener(Self::key_down))
            // Clicks and the wheel stop here, never reaching the panes.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(sheet_title("New agent"))
            .children(presets)
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
                    .child(self.command.clone())
                    .children(recent_commands),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SM - space::XXS))
                    .child(field_label("Prompt", &theme))
                    .child(self.prompt.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(space::SM))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(space::SM - space::XXS))
                            .child(field_label("Name", &theme))
                            .child(self.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(space::SM - space::XXS))
                            .child(field_label("Labels", &theme))
                            .child(self.labels.clone()),
                    ),
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
                        tab_stop(
                            button("cancel", "Cancel", &theme, ButtonKind::Secondary, true),
                            &theme,
                        )
                        .debug_selector(|| "new-agent-cancel".to_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        tab_stop(
                            button("spawn", "Spawn", &theme, ButtonKind::Primary, true),
                            &theme,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{dir_choices, dir_hint};
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
    fn dirs_are_recent_then_agent_cwds_newest_first_distinct() {
        let recent = vec!["/a".to_owned(), "/b".to_owned()];
        let agents = [row("/old", 1), row("/b", 5), row("/new", 9), row("/new", 3)];
        assert_eq!(dir_choices(&recent, &agents), ["/a", "/b", "/new", "/old"]);
    }
}
