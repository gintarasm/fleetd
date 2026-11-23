//! The diff pane (ADR 0094): what an agent changed, read-only, in a split
//! beside its terminal. It asks `git-worktree.diff` for a `DiffView` on
//! open, on ⌘R or the refresh button, and once when its agent turns
//! waiting, done or ended while it is shown; never on a timer or output.
//! `core::diff` lays the view out once per refresh, and the pane draws
//! only the rows in its viewport (a uniform-height list).

use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use fleet_proto::action_result::Outcome;
use fleet_proto::{ActionResult, DiffView, RunAction, view};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, BoxShadow, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Font,
    InteractiveElement, IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, actions, div, point, px,
    uniform_list,
};

use super::app_state::SessionApi;
use super::controls::icon_button;
use super::theme::{Theme, Typography, current as theme};
use crate::core::diff::{DiffDoc, Row, RowKind, stat_text};
use crate::core::icons::Icon;
use crate::core::style::{radius, space, text};
use crate::core::worktree::{AGAINST, DIFF, diff_params};

actions!(diff_pane, [RefreshDiff, CopyDiff, SelectAllDiff]);

/// Key context of a diff pane: its ⌘R (refresh) and ⌘C (copy) win over the
/// workspace's ⌘R (restart) while it has the keyboard.
pub const DIFF_CONTEXT: &str = "DiffPane";

/// The diff pane's own keys.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-r", RefreshDiff, Some(DIFF_CONTEXT)),
        KeyBinding::new("cmd-c", CopyDiff, Some(DIFF_CONTEXT)),
        KeyBinding::new("cmd-a", SelectAllDiff, Some(DIFF_CONTEXT)),
    ]);
}

/// Width of the file list.
const FILE_LIST_WIDTH: f32 = 220.0;
/// The gutters' width in characters (line numbers up to 99,999).
const GUTTER_CHARS: f32 = 6.0;

/// What the pane tells the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffPaneEvent {
    /// Close the pane (its ✕).
    Close,
}

/// What the pane shows.
#[derive(Debug)]
enum Shown {
    Loading,
    Diff(DiffDoc),
    /// The action failed or refused: the reason.
    Failed(String),
}

pub struct DiffPane {
    pub agent_id: u64,
    /// Index into [`AGAINST`].
    pub against: usize,
    /// The agent's name, for the header while loading.
    pub agent_name: String,
    shown: Shown,
    /// A refresh is on its way (the old diff stays shown meanwhile).
    pub loading: bool,
    /// Bumped by every request; a reply for an older one is dropped.
    request: u64,
    /// How many diffs arrived (tests and the benchmark).
    pub loads: u64,
    session: Arc<dyn SessionApi>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    files_scroll: UniformListScrollHandle,
    /// The file ↑/↓ moves through.
    pub selected_file: usize,
    /// Selected rows: anchor and head (shown-row indices).
    selection: Option<(usize, usize)>,
    dragging: bool,
    font: Font,
    font_size: Pixels,
    /// When the pane was asked to open (`gui_diff_open_ns`).
    pub(crate) opened_at: Option<Instant>,
}

impl EventEmitter<DiffPaneEvent> for DiffPane {}

impl Focusable for DiffPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DiffPane {
    pub fn new(
        agent_id: u64,
        agent_name: String,
        session: Arc<dyn SessionApi>,
        font: Font,
        font_size: Pixels,
        cx: &mut Context<Self>,
    ) -> Self {
        DiffPane {
            agent_id,
            against: 0,
            agent_name,
            shown: Shown::Loading,
            loading: false,
            request: 0,
            loads: 0,
            session,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            files_scroll: UniformListScrollHandle::new(),
            selected_file: 0,
            selection: None,
            dragging: false,
            font,
            font_size,
            opened_at: None,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The laid-out diff, once one arrived.
    pub fn doc(&self) -> Option<&DiffDoc> {
        match &self.shown {
            Shown::Diff(d) => Some(d),
            _ => None,
        }
    }

    /// Why there is no diff, when the action failed.
    pub fn failure(&self) -> Option<&str> {
        match &self.shown {
            Shown::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn set_font(&mut self, font: Font, size: Pixels, cx: &mut Context<Self>) {
        self.font = font;
        self.font_size = size;
        cx.notify();
    }

    /// Asks for the diff again (the old one stays until the new arrives).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request(None, cx);
    }

    /// Shows the diff against `AGAINST[i]`.
    pub fn set_against(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < AGAINST.len() && i != self.against {
            self.against = i;
            self.refresh(cx);
        }
    }

    /// Puts a diff in, as a reply to the newest request would (the
    /// benchmark and tests).
    pub fn show_view(&mut self, view: &DiffView, cx: &mut Context<Self>) {
        self.request += 1;
        self.apply(None, Ok(diff_result(view.clone())), cx);
    }

    fn request(&mut self, file: Option<(usize, String)>, cx: &mut Context<Self>) {
        self.request += 1;
        let request = self.request;
        if file.is_none() {
            self.loading = true;
        }
        let params = diff_params(
            self.agent_id,
            AGAINST[self.against],
            file.as_ref().map(|(_, k)| k.as_str()),
        );
        let job = self.session.run_action(RunAction {
            name: DIFF.to_owned(),
            params,
            cwd: String::new(),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |p, cx| {
                // A file's hunks still fit a newer diff of the same files;
                // a whole diff only if it is the newest asked for.
                if file.is_none() && request != p.request {
                    return;
                }
                p.apply(file.map(|(i, _)| i), result, cx);
            });
        })
        .detach();
    }

    fn apply(
        &mut self,
        file: Option<usize>,
        result: Result<ActionResult, String>,
        cx: &mut Context<Self>,
    ) {
        let view = match result.map(|r| r.outcome) {
            Ok(Some(Outcome::View(fleet_proto::View {
                kind: Some(view::Kind::Diff(d)),
            }))) => Ok(d),
            Ok(Some(Outcome::Refused(r))) => Err(r.reason),
            Ok(Some(Outcome::Done(d))) => Err(d.message),
            Ok(_) => Err("the diff action answered something else".to_owned()),
            Err(e) => Err(e),
        };
        match (file, view) {
            (Some(i), Ok(v)) => {
                if let (Shown::Diff(doc), Some(f)) = (&mut self.shown, v.files.first()) {
                    doc.replace_file(i, f);
                }
            }
            (Some(_), Err(e)) => {
                eprintln!("fleet-gui: cannot load the file's diff: {e}");
            }
            (None, Ok(v)) => {
                let mut doc = DiffDoc::new(&v);
                // A refresh keeps what the user collapsed.
                if let Shown::Diff(old) = &self.shown {
                    for (i, f) in doc.files().to_vec().iter().enumerate() {
                        let was = old.files().iter().find(|o| o.key == f.key);
                        if was.is_some_and(|o| o.collapsed && !o.omitted) && !f.collapsed {
                            doc.toggle(i);
                        }
                    }
                }
                self.selected_file = self.selected_file.min(doc.files().len().saturating_sub(1));
                self.selection = None;
                self.shown = Shown::Diff(doc);
                self.loading = false;
                self.loads += 1;
            }
            (None, Err(e)) => {
                self.shown = Shown::Failed(e);
                self.loading = false;
                self.loads += 1;
            }
        }
        cx.notify();
    }

    /// Collapses or expands file `i`; an omitted file is asked for.
    pub fn toggle_file(&mut self, i: usize, cx: &mut Context<Self>) {
        let Shown::Diff(doc) = &mut self.shown else {
            return;
        };
        self.selection = None;
        if doc.toggle(i) {
            let key = doc.files()[i].key.clone();
            self.request(Some((i, key)), cx);
        }
        cx.notify();
    }

    /// Selects file `i` and scrolls its header to the top.
    pub fn select_file(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(doc) = self.doc() else { return };
        if i >= doc.files().len() {
            return;
        }
        if let Some(row) = doc.header_row(i) {
            self.scroll.scroll_to_item_strict(row, ScrollStrategy::Top);
        }
        self.files_scroll.scroll_to_item(i, ScrollStrategy::Center);
        self.selected_file = i;
        cx.notify();
    }

    /// The selected rows' text, as a unified diff writes them.
    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        Some(self.doc()?.copy_text(a, b))
    }

    fn copy(&mut self, _: &CopyDiff, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn select_all(&mut self, _: &SelectAllDiff, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.doc().map(DiffDoc::len).filter(|n| *n > 0) {
            self.selection = Some((0, n - 1));
            cx.notify();
        }
    }

    fn refresh_action(&mut self, _: &RefreshDiff, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.platform || m.control || m.alt {
            return;
        }
        let files = self.doc().map_or(0, |d| d.files().len());
        if files == 0 {
            return;
        }
        match ev.keystroke.key.as_str() {
            "up" => self.select_file(self.selected_file.saturating_sub(1), cx),
            "down" => self.select_file((self.selected_file + 1).min(files - 1), cx),
            "space" => {
                let i = self.selected_file;
                self.toggle_file(i, cx);
                self.select_file(i, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn press_row(&mut self, i: usize, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(row) = self.doc().and_then(|d| d.row(i)).cloned() else {
            return;
        };
        if row.kind == RowKind::File && !ev.modifiers.shift {
            self.selected_file = row.file;
            self.toggle_file(row.file, cx);
            return;
        }
        self.selected_file = row.file;
        self.selection = match (self.selection, ev.modifiers.shift) {
            (Some((anchor, _)), true) => Some((anchor, i)),
            _ => Some((i, i)),
        };
        self.dragging = true;
        cx.notify();
    }

    fn drag_row(&mut self, i: usize, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.dragging || ev.pressed_button != Some(MouseButton::Left) {
            self.dragging = false;
            return;
        }
        if let Some((anchor, head)) = self.selection
            && head != i
        {
            self.selection = Some((anchor, i));
            cx.notify();
        }
    }

    fn row_height(&self) -> Pixels {
        (self.font_size * 1.45).round()
    }

    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (title, stats, from) = match &self.shown {
            Shown::Diff(d) => (
                if d.title.is_empty() {
                    self.agent_name.clone()
                } else {
                    d.title.clone()
                },
                format!(
                    "{} file{} · {}",
                    d.files_changed,
                    if d.files_changed == 1 { "" } else { "s" },
                    stat_text(d.additions, d.deletions)
                ),
                if d.from_label.is_empty() {
                    String::new()
                } else {
                    format!("{} \u{2192} {}", d.from_label, d.to_label)
                },
            ),
            _ => (self.agent_name.clone(), String::new(), String::new()),
        };
        let against = AGAINST.iter().enumerate().map(|(i, name)| {
            let on = i == self.against;
            let t = *theme;
            div()
                .id(("diff-against", i))
                .debug_selector(move || format!("diff-against-{name}"))
                .px(px(space::SM))
                .py(px(space::XXS))
                .rounded(px(radius::SM))
                .type_style(text::SUBHEADLINE)
                .cursor_pointer()
                .when(on, |d| d.bg(t.selected).text_color(t.text))
                .when(!on, |d| d.text_color(t.secondary).hover(move |s| s.bg(t.hover)))
                .child(*name)
                .on_click(cx.listener(move |p, _, _, cx| p.set_against(i, cx)))
        });
        div()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(space::SM))
            .px(px(space::MD))
            .py(px(space::XS))
            .border_b_1()
            .border_color(theme.separator)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .type_style(text::HEADLINE)
                            .truncate()
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .type_style(text::SUBHEADLINE)
                            .text_color(theme.secondary)
                            .truncate()
                            .child(SharedString::from(
                                [from, stats]
                                    .into_iter()
                                    .filter(|s| !s.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .gap(px(space::XXS))
                    .p(px(space::XXS))
                    .rounded(px(radius::MD))
                    .bg(theme.field)
                    .children(against),
            )
            .child(
                icon_button("diff-refresh", Icon::Restart, theme, true)
                    .debug_selector(|| "diff-refresh".to_owned())
                    .when(self.loading, |b| b.opacity(0.5))
                    .on_click(cx.listener(|p, _, _, cx| p.refresh(cx))),
            )
            .child(
                icon_button("diff-close", Icon::Close, theme, true)
                    .debug_selector(|| "diff-close".to_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffPaneEvent::Close))),
            )
    }

    fn render_files(&self, doc: &DiffDoc, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = *theme;
        let count = doc.files().len();
        let row_h = px(22.0);
        div()
            .flex_none()
            .w(px(FILE_LIST_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(theme.separator)
            .child(
                uniform_list(
                    "diff-files",
                    count,
                    cx.processor(move |p, range: Range<usize>, _, cx| {
                        let Some(doc) = p.doc() else { return Vec::new() };
                        range
                            .filter_map(|i| {
                                let f = doc.files().get(i)?;
                                let on = i == p.selected_file;
                                let status_color = match f.status {
                                    'A' => t.success,
                                    'D' => t.danger,
                                    _ => t.warning,
                                };
                                Some(
                                    div()
                                        .id(("diff-file", i))
                                        .h(row_h)
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(space::XS))
                                        .px(px(space::SM))
                                        .type_style(text::CALLOUT)
                                        .cursor_pointer()
                                        .when(on, |d| d.bg(t.selected))
                                        .when(!on, |d| d.hover(move |s| s.bg(t.hover)))
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(px(12.0))
                                                .text_color(status_color)
                                                .child(SharedString::from(f.status.to_string())),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(SharedString::from(f.path.clone())),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .type_style(text::CAPTION)
                                                .text_color(t.secondary)
                                                .child(SharedString::from(stat_text(
                                                    f.additions.into(),
                                                    f.deletions.into(),
                                                ))),
                                        )
                                        .on_click(cx.listener(move |p, _, window, cx| {
                                            window.focus(&p.focus);
                                            p.select_file(i, cx);
                                        })),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.files_scroll.clone())
                .size_full(),
            )
    }

    fn render_rows(&self, doc: &DiffDoc, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = *theme;
        let row_h = self.row_height();
        let char_w = self.font_size * 0.62;
        let gutter = char_w * GUTTER_CHARS;
        let font = self.font.clone();
        let font_size = self.font_size;
        let count = doc.len();
        // The sticky header: the file of the top row, unless that row is
        // its own header.
        let offset = -self.scroll.0.borrow().base_handle.offset().y;
        let top = if row_h > px(0.) {
            (offset / row_h).floor().max(0.0) as usize
        } else {
            0
        };
        let sticky = doc
            .file_at(top)
            .filter(|_| doc.row(top).is_some_and(|r| r.kind != RowKind::File))
            .and_then(|f| Some((f, doc.header_row(f)?)))
            .and_then(|(f, at)| Some((f, doc.row(at)?.clone())));
        let list = uniform_list(
            "diff-rows",
            count,
            cx.processor(move |p, range: Range<usize>, _, cx| {
                let Some(doc) = p.doc() else { return Vec::new() };
                let sel = p.selection.map(|(a, b)| (a.min(b), a.max(b)));
                range
                    .filter_map(|i| {
                        let row = doc.row(i)?;
                        let selected = sel.is_some_and(|(a, b)| (a..=b).contains(&i));
                        let collapsed = doc.files().get(row.file).is_some_and(|f| f.collapsed);
                        Some(
                            row_element(row, collapsed, selected, row_h, gutter, &t)
                                .id(("diff-row", i))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |p, ev: &MouseDownEvent, window, cx| {
                                        p.press_row(i, ev, window, cx);
                                    }),
                                )
                                .on_mouse_move(cx.listener(move |p, ev: &MouseMoveEvent, _, cx| {
                                    p.drag_row(i, ev, cx);
                                }))
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|p, _, _, _| p.dragging = false),
                                ),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .size_full();
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .font(font)
            .text_size(font_size)
            .line_height(row_h)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .child(list)
            .children(sticky.map(|(f, row)| {
                let collapsed = doc.files().get(f).is_some_and(|f| f.collapsed);
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .debug_selector(|| "diff-sticky".to_owned())
                    .shadow(vec![BoxShadow {
                        color: t.shadow,
                        offset: point(px(0.), px(1.)),
                        blur_radius: px(space::XS),
                        spread_radius: px(0.),
                    }])
                    .child(
                        row_element(&row, collapsed, false, row_h, gutter, &t)
                            .id("diff-sticky-row")
                            .on_click(cx.listener(move |p, _, _, cx| {
                                p.toggle_file(f, cx);
                                p.select_file(f, cx);
                            })),
                    )
            }))
    }
}

/// One display row: gutters, marker and text, on its colour.
fn row_element(
    row: &Row,
    collapsed: bool,
    selected: bool,
    height: Pixels,
    gutter: Pixels,
    t: &Theme,
) -> gpui::Div {
    let number = |n: Option<u32>| {
        div()
            .flex_none()
            .w(gutter)
            .pr(px(space::XS))
            .flex()
            .justify_end()
            .text_color(t.tertiary)
            .child(SharedString::from(n.map(|n| n.to_string()).unwrap_or_default()))
    };
    let (bg, fg, marker) = match row.kind {
        RowKind::Added => (Some(t.success.opacity(0.14)), t.text, "+"),
        RowKind::Removed => (Some(t.danger.opacity(0.14)), t.text, "-"),
        RowKind::Context => (None, t.text, " "),
        RowKind::Hunk => (Some(t.accent_wash), t.secondary, ""),
        RowKind::NoNewline | RowKind::Note => (None, t.secondary, ""),
        RowKind::File => (Some(t.raised), t.text, ""),
    };
    let base = div()
        .h(height)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(fg)
        .when_some(bg, |d, bg| d.bg(bg))
        .when(selected, |d| d.bg(t.selection));
    match row.kind {
        RowKind::File => base
            .px(px(space::SM))
            .gap(px(space::XS))
            .border_b_1()
            .border_color(t.separator)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .cursor_pointer()
            .child(if collapsed { "\u{25B8}" } else { "\u{25BE}" })
            .child(SharedString::from(row.text.clone())),
        RowKind::Note => base
            .child(number(None))
            .child(number(None))
            .child(div().italic().child(SharedString::from(row.text.clone()))),
        _ => base
            .child(number(row.old))
            .child(number(row.new))
            .child(div().flex_none().w(px(12.0)).child(marker))
            .child(SharedString::from(row.text.clone())),
    }
}

/// A `view` result holding `view`.
pub fn diff_result(view: DiffView) -> ActionResult {
    ActionResult {
        outcome: Some(Outcome::View(fleet_proto::View {
            kind: Some(view::Kind::Diff(view)),
        })),
    }
}

impl Render for DiffPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let focused = self.focus.contains_focused(window, cx);
        if self.doc().is_some()
            && let Some(at) = self.opened_at.take()
        {
            super::bench::diff_opened(at);
        }
        let header = self.render_header(&theme, cx);
        let body = match &self.shown {
            Shown::Diff(doc) if doc.files().is_empty() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .type_style(text::BODY)
                .text_color(theme.tertiary)
                .child("No changes")
                .into_any_element(),
            Shown::Diff(doc) => {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_files(doc, &theme, cx))
                    .child(self.render_rows(doc, &theme, cx))
                    .into_any_element()
            }
            Shown::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .type_style(text::BODY)
                .text_color(theme.tertiary)
                .child("Loading changes…")
                .into_any_element(),
            Shown::Failed(e) => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(space::SM))
                .px(px(space::LG))
                .type_style(text::BODY)
                .text_color(theme.secondary)
                .debug_selector(|| "diff-failed".to_owned())
                .child("Cannot show the changes")
                .child(
                    div()
                        .type_style(text::CALLOUT)
                        .text_color(theme.tertiary)
                        .child(SharedString::from(e.clone())),
                )
                .into_any_element(),
        };
        div()
            .id("diff-pane")
            .debug_selector(|| "diff-pane".to_owned())
            .key_context(DIFF_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::refresh_action))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|p, _, window, _| window.focus(&p.focus)),
            )
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(radius::LG))
            .overflow_hidden()
            .shadow(vec![BoxShadow {
                color: theme.shadow,
                offset: point(px(0.), px(1.)),
                blur_radius: px(space::XS),
                spread_radius: px(0.),
            }])
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .bg(theme.card)
            .child(header)
            .child(body)
    }
}
