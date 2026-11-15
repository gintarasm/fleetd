//! The New agent sheet: a directory (typed, picked from the recent ones,
//! or chosen with the native folder picker for a local daemon) and a
//! command, prefilled from `[gui] default_command`.

use std::ops::Range;

use gpui::{
    App, AppContext, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, PathPromptOptions, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement, Styled, UTF16Selection, Window, canvas, div, px,
};

use super::controls::{
    ButtonKind, button, error_text, field_label, list_row, list_well, sheet_card, sheet_title,
};
use super::keymap::{Paste, TEXT_FIELD_CONTEXT};
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

/// A one-line text field. Text goes in through the input handler (typing,
/// IME), the clipboard or [`TextField::set_text`]; Backspace deletes the
/// last character. There is no caret movement: edits happen at the end.
pub struct TextField {
    text: String,
    /// The IME's uncommitted text, a UTF-16 range of `text`.
    marked: Option<Range<usize>>,
    focus: FocusHandle,
    placeholder: SharedString,
}

impl TextField {
    pub fn new(text: String, placeholder: &str, cx: &mut Context<Self>) -> Self {
        TextField {
            text,
            marked: None,
            focus: cx.focus_handle(),
            placeholder: SharedString::from(placeholder.to_owned()),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.text = text;
        self.marked = None;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }

    /// The byte offset of UTF-16 offset `u16_at`, clamped to the text.
    fn byte_at(&self, u16_at: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.text.char_indices() {
            if seen >= u16_at {
                return i;
            }
            seen += c.len_utf16();
        }
        self.text.len()
    }

    /// Replaces `range` (UTF-16; default: the marked text, else the end)
    /// and returns the UTF-16 range of what was inserted.
    fn replace(&mut self, range: Option<Range<usize>>, new: &str) -> Range<usize> {
        let end = self.utf16_len();
        let range = range.or_else(|| self.marked.clone()).unwrap_or(end..end);
        let start = range.start.min(end);
        let stop = range.end.clamp(start, end);
        let (from, to) = (self.byte_at(start), self.byte_at(stop));
        self.text.replace_range(from..to, new);
        start..start + new.encode_utf16().count()
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if ev.keystroke.key == "backspace" && self.marked.is_none() && !m.platform {
            self.text.pop();
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            let line = text.lines().next().unwrap_or("").to_owned();
            self.replace(None, &line);
            self.marked = None;
            cx.notify();
        }
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let focused = self.focus.is_focused(window);
        let (shown, color) = if self.text.is_empty() {
            (self.placeholder.clone(), theme.tertiary)
        } else {
            (SharedString::from(self.text.clone()), theme.text)
        };
        let entity = cx.entity();
        let focus = self.focus.clone();
        div()
            .key_context(TEXT_FIELD_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::paste))
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .h(px(26.))
            .px(px(6.))
            .rounded(px(4.))
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .bg(theme.field)
            .overflow_hidden()
            .child(div().text_color(color).whitespace_nowrap().child(shown))
            // The caret, after the text.
            .children(focused.then(|| div().w(px(1.)).h(px(14.)).bg(theme.text)))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let len = self.utf16_len();
        let start = range.start.min(len);
        let end = range.end.clamp(start, len);
        *adjusted_range = Some(start..end);
        Some(self.text[self.byte_at(start)..self.byte_at(end)].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.utf16_len();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, text);
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inserted = self.replace(range, new_text);
        self.marked = (!inserted.is_empty()).then_some(inserted);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
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
