//! The pane's mouse: selection, reporting to the agent, links, the
//! overlay scrollbar and the wheel.

use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, Modifiers, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, Window, point, px, size,
};

use super::{GridLayout, PaneEvent, PaneView, lock};
use crate::core::keys::KeyMods;
use crate::core::links::{Files, Link, LinkTarget, Osc8, editor_command, link_at, pick_editor};
use crate::core::mouse::{
    DRAG_SCROLL_MS, MouseButton, MouseKind, drag_scroll_lines, encode_mouse, wheel_as_arrows,
};
use crate::core::pane::{PaneModel, ViewRow};
use crate::core::scroll_feedback::{HIT_WIDTH, INSET, Thumb, Wake, offset_for_thumb, thumb};
use crate::core::selection::{LineRef, SelPoint};

/// At most this many lines are scrolled (or reported) per wheel event.
const MAX_WHEEL_LINES: f32 = 100.0;

/// The `(col, row)` of the cell under `pos` in a grid of `cols`×`rows`
/// cells at `bounds.origin`, clamped inside the grid.
pub fn point_to_cell(
    pos: Point<Pixels>,
    bounds: Bounds<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    cols: u16,
    rows: u16,
) -> (u16, u16) {
    let fit = |offset: Pixels, cell: Pixels, n: u16| {
        let at = f32::from(offset) / f32::from(cell);
        if at.is_finite() && at > 0.0 {
            // `as` saturates; the `min` keeps it on the grid.
            (at.floor() as u16).min(n.saturating_sub(1))
        } else {
            0
        }
    };
    (
        fit(pos.x - bounds.origin.x, cell_w, cols),
        fit(pos.y - bounds.origin.y, line_h, rows),
    )
}

/// What a held mouse button is doing.
#[derive(Clone, Copy, Debug)]
pub(super) enum Drag {
    /// Extending the selection.
    Select,
    /// Reported to the agent; `at` is the cell last reported.
    Report { button: MouseButton, at: (u16, u16) },
    /// Dragging the scrollbar's thumb, held `grab` points below its top.
    Thumb { grab: f32 },
}

impl PaneView {
    /// The link underlined under the pointer: its line and columns.
    #[cfg(test)]
    pub(crate) fn hovered_link(&self) -> Option<(LineRef, u16, u16)> {
        self.hover
    }

    /// The link under `pos`, if any, on the line it is on.
    fn link_under(&self, pos: Point<Pixels>) -> Option<(LineRef, Link)> {
        let l = self.layout?;
        let model = self.model();
        let term = model.terminal();
        let (cols, rows) = term.size();
        let grid = Bounds::new(
            l.origin,
            size(l.cell_w * f32::from(cols), l.line_h * f32::from(rows)),
        );
        if !grid.contains(&pos) {
            return None;
        }
        let (col, row) = point_to_cell(pos, grid, l.cell_w, l.line_h, cols, rows);
        let mut cells = Vec::with_capacity(usize::from(cols));
        let (line, osc8) = match model.view_row(row) {
            ViewRow::History(abs) => {
                model.history_cells(abs, &mut cells);
                (LineRef::History(abs), None)
            }
            ViewRow::Grid(r) => {
                term.row_cells(r, &mut cells);
                (LineRef::Grid(r), term.hyperlink_at(r, col))
            }
        };
        let osc8 = osc8.as_ref().map(|(uri, start, end)| Osc8 {
            uri,
            start: *start,
            end: *end,
        });
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let is_file = |p: &std::path::Path| p.is_file();
        let files = self.chrome.link_dir.as_deref().map(|cwd| Files {
            cwd: std::path::Path::new(cwd),
            home: home.as_deref(),
            is_file: &is_file,
        });
        link_at(&cells, col, osc8, files.as_ref()).map(|link| (line, link))
    }

    /// Underlines the link under `pos` while ⌘ is held and no drag is on.
    fn update_hover(&mut self, pos: Point<Pixels>, m: Modifiers, cx: &mut Context<Self>) {
        let hover = (m.platform && self.drag.is_none())
            .then(|| self.link_under(pos))
            .flatten()
            .map(|(line, l)| (line, l.start, l.end));
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    pub(crate) fn modifiers_changed(
        &mut self,
        m: Modifiers,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.update_hover(pos, m, cx);
    }

    /// Opens a link: URLs through the system, files in the editor.
    fn open_link(&self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Url(url) => cx.open_url(&url),
            LinkTarget::File { path, line, col } => {
                let visual = std::env::var("VISUAL").ok();
                let env_editor = std::env::var("EDITOR").ok();
                let editor = pick_editor(&self.editor, visual.as_deref(), env_editor.as_deref());
                let argv = editor_command(editor.as_deref(), &path, line, col);
                (self.open_file)(argv, editor_command(None, &path, None, None));
            }
        }
    }

    /// The strip along the grid's right edge that the scrollbar takes
    /// presses in, in window coordinates.
    pub(crate) fn scrollbar_track(&self) -> Option<Bounds<Pixels>> {
        let l = self.layout?;
        let height = l.size.height - px(2.0 * INSET);
        (height > px(0.)).then(|| {
            Bounds::new(
                point(
                    l.origin.x + l.size.width - px(HIT_WIDTH),
                    l.origin.y + px(INSET),
                ),
                size(px(HIT_WIDTH), height),
            )
        })
    }

    /// The thumb on `track` for the view as it is.
    pub(super) fn thumb_on(&self, track: Bounds<Pixels>) -> Option<Thumb> {
        let model = self.model();
        let rows = model.terminal().size().1;
        thumb(
            f32::from(track.size.height),
            model.scrollback().len(),
            rows,
            model.scroll_offset(),
        )
    }

    /// The view scrolled: the scrollbar shows (again) from now.
    pub(super) fn show_scrollbar(&mut self) {
        if !self.fade.is_active() {
            self.fade_reduce_motion = (self.reduce_motion)();
        }
        self.fade.show(Instant::now());
    }

    /// The scrollbar's opacity this render, scheduling what keeps it
    /// current: frames while it fades, one timer to the end of its hold,
    /// nothing once it is gone (or never shown).
    pub(super) fn scrollbar_opacity(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<f32> {
        if !self.fade.is_active() {
            return None;
        }
        let now = Instant::now();
        let rm = self.fade_reduce_motion;
        let opacity = self.fade.opacity(now, rm);
        match self.fade.wake(now, rm) {
            Wake::Frame => window.request_animation_frame(),
            Wake::After(d) => {
                let due = now + d;
                if self.fade_timer.as_ref().is_none_or(|(at, _)| *at != due) {
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(d).await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    });
                    self.fade_timer = Some((due, task));
                }
            }
            Wake::Idle => {
                self.fade.settle(now, rm);
                self.fade_timer = None;
            }
        }
        (opacity > 0.0).then_some(opacity)
    }

    /// Moves the view so the dragged thumb's top is `grab` above `y`.
    fn drag_thumb(&mut self, y: Pixels, grab: f32, cx: &mut Context<Self>) {
        let Some(track) = self.scrollbar_track() else {
            return;
        };
        let Some(t) = self.thumb_on(track) else {
            return;
        };
        let top = f32::from(y - track.origin.y) - grab;
        let track_h = f32::from(track.size.height);
        {
            let mut model = self.model();
            let history = model.scrollback().len();
            model.scroll_to(offset_for_thumb(track_h, t.height, history, top));
        }
        self.sink.check_paging(self.pane);
        cx.notify();
    }

    /// The pointer let go of the scrollbar: it fades after a new hold.
    fn release_scrollbar(&mut self, cx: &mut Context<Self>) {
        self.fade.hold(false);
        self.show_scrollbar();
        cx.notify();
    }

    pub(crate) fn mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        if self.chip.is_some_and(|p| p.contains(&ev.position)) {
            self.drag = None;
            self.model().scroll_to_bottom();
            cx.notify();
            return;
        }
        let Some(button) = to_button(ev.button) else {
            return;
        };
        // A shown scrollbar takes presses on its strip: on the thumb it is
        // dragged from there, elsewhere the thumb jumps under the pointer.
        let track = self.scrollbar_track().filter(|t| {
            self.scrollbar_shown && button == MouseButton::Left && t.contains(&ev.position)
        });
        if let Some((track, t)) = track.and_then(|tr| self.thumb_on(tr).map(|t| (tr, t))) {
            let y = f32::from(ev.position.y - track.origin.y);
            let on_thumb = y >= t.top && y <= t.top + t.height;
            let grab = if on_thumb { y - t.top } else { t.height / 2.0 };
            self.drag = Some(Drag::Thumb { grab });
            self.fade.hold(true);
            if !on_thumb {
                self.drag_thumb(ev.position.y, grab, cx);
            }
            cx.notify();
            return;
        }
        // ⌘-click on a link opens it, even when the agent reports the mouse.
        let link = (button == MouseButton::Left && ev.modifiers.platform)
            .then(|| self.link_under(ev.position))
            .flatten();
        if let Some((_, link)) = link {
            self.drag = None;
            self.open_link(link.target, cx);
            return;
        }
        let mut model = lock(&self.model);
        let Some(at) = cell_at(self.layout, &model, ev.position) else {
            return;
        };
        if reports(&model, ev.modifiers) {
            let modes = model.terminal().modes();
            drop(model);
            self.drag = Some(Drag::Report { button, at });
            let bytes = encode_mouse(
                button,
                MouseKind::Press,
                at.0,
                at.1,
                mods(ev.modifiers),
                &modes,
            );
            if let Some(bytes) = bytes {
                self.send(bytes, cx);
            }
            return;
        }
        if button == MouseButton::Right {
            drop(model);
            cx.emit(PaneEvent::ContextMenu { on_grid: true });
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let p = sel_point(&model, at);
        match ev.click_count {
            // Shift-click moves the selection's end here (decision 0095).
            0 | 1 if ev.modifiers.shift && model.has_selection() => model.select_extend(p),
            0 | 1 => model.select_start(p),
            2 => model.select_word(p),
            _ => model.select_line(p),
        }
        drop(model);
        self.drag = Some(Drag::Select);
        self.drag_at = ev.position;
        cx.notify();
    }

    pub(crate) fn mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            // Over a showing scrollbar, it stays.
            if self.fade.is_active() {
                let over = self.scrollbar_shown
                    && self
                        .scrollbar_track()
                        .is_some_and(|t| t.contains(&ev.position));
                if over != self.fade.is_held() {
                    if over {
                        self.fade.hold(true);
                    } else {
                        self.release_scrollbar(cx);
                    }
                }
            }
            self.update_hover(ev.position, ev.modifiers, cx);
            return;
        };
        let pressed = ev.pressed_button.and_then(to_button);
        if let Drag::Thumb { grab } = drag {
            if pressed == Some(MouseButton::Left) {
                self.drag_thumb(ev.position.y, grab, cx);
            } else {
                // The release happened where it was not seen.
                self.drag = None;
                self.release_scrollbar(cx);
            }
            return;
        }
        let mut model = lock(&self.model);
        let Some(at) = cell_at(self.layout, &model, ev.position) else {
            return;
        };
        match drag {
            // Handled above, without the model locked.
            Drag::Thumb { .. } => {}
            // The release happened where it was not seen.
            Drag::Select if pressed != Some(MouseButton::Left) => self.drag = None,
            Drag::Select => {
                let p = sel_point(&model, at);
                model.select_extend(p);
                drop(model);
                self.drag_at = ev.position;
                self.auto_scroll_from_edge(cx);
                cx.notify();
            }
            Drag::Report { button, .. } if pressed != Some(button) => self.drag = None,
            Drag::Report { at: last, .. } if last == at => {}
            Drag::Report { button, .. } => {
                let modes = model.terminal().modes();
                drop(model);
                self.drag = Some(Drag::Report { button, at });
                let kind = MouseKind::Drag;
                if let Some(bytes) =
                    encode_mouse(button, kind, at.0, at.1, mods(ev.modifiers), &modes)
                {
                    self.send(bytes, cx);
                }
            }
        }
    }

    /// Lines a drag-select tick scrolls with the pointer where it was last
    /// seen: none while it is over the grid (decision 0095).
    fn drag_scroll_lines(&self) -> i32 {
        let Some(l) = self.layout else {
            return 0;
        };
        let rows = self.model().terminal().size().1;
        let top = f32::from(l.origin.y);
        let bottom = top + f32::from(l.line_h) * f32::from(rows);
        drag_scroll_lines(f32::from(self.drag_at.y), top, bottom, f32::from(l.line_h))
    }

    /// Starts the repeating drag-select scroll once the pointer is past
    /// the grid's top or bottom; one timer, stopped by the tick that finds
    /// the drag over or the pointer back over the grid.
    fn auto_scroll_from_edge(&mut self, cx: &mut Context<Self>) {
        if self.auto_scroll.is_some() || self.drag_scroll_lines() == 0 {
            return;
        }
        self.auto_scroll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(DRAG_SCROLL_MS))
                    .await;
                let more = this.update(cx, |this, cx| this.auto_scroll_tick(cx));
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// One drag-select scroll: moves the view by the pointer's distance
    /// past the edge and extends the selection to the row at that edge.
    /// `false` once the drag ended or the pointer is back over the grid.
    fn auto_scroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let lines = self.drag_scroll_lines();
        if !matches!(self.drag, Some(Drag::Select)) || lines == 0 {
            self.auto_scroll = None;
            return false;
        }
        let mut model = lock(&self.model);
        let col = cell_at(self.layout, &model, self.drag_at).map_or(0, |(c, _)| c);
        let moved = model.drag_scroll(lines, col);
        drop(model);
        if moved {
            self.show_scrollbar();
            self.sink.check_paging(self.pane);
            cx.notify();
        }
        true
    }

    pub(crate) fn mouse_up(
        &mut self,
        ev: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        if let Drag::Thumb { .. } = drag {
            self.drag = None;
            self.release_scrollbar(cx);
            return;
        }
        let Drag::Report { button, at: last } = drag else {
            self.drag = None;
            return;
        };
        if to_button(ev.button) != Some(button) {
            return;
        }
        self.drag = None;
        let model = lock(&self.model);
        let at = cell_at(self.layout, &model, ev.position).unwrap_or(last);
        let modes = model.terminal().modes();
        drop(model);
        let kind = MouseKind::Release;
        if let Some(bytes) = encode_mouse(button, kind, at.0, at.1, mods(ev.modifiers), &modes) {
            self.send(bytes, cx);
        }
    }

    pub(crate) fn scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.layout else {
            return;
        };
        let lines = match ev.delta {
            ScrollDelta::Lines(d) => d.y,
            ScrollDelta::Pixels(d) => f32::from(d.y) / f32::from(layout.line_h),
        };
        if !lines.is_finite() || lines == 0.0 {
            return;
        }
        // A change of direction drops what the other way left over.
        if self.wheel_lines != 0.0 && self.wheel_lines.signum() != lines.signum() {
            self.wheel_lines = 0.0;
        }
        self.wheel_lines += lines;
        let whole = self.wheel_lines.trunc();
        self.wheel_lines -= whole;
        let lines = whole.clamp(-MAX_WHEEL_LINES, MAX_WHEEL_LINES) as i32;
        if lines == 0 {
            return;
        }

        let mut model = lock(&self.model);
        let modes = model.terminal().modes();
        let bytes = if reports(&model, ev.modifiers) {
            let (col, row) = cell_at(self.layout, &model, ev.position).unwrap_or((0, 0));
            let button = if lines > 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            encode_mouse(
                button,
                MouseKind::Press,
                col,
                row,
                mods(ev.modifiers),
                &modes,
            )
            .map(|one| one.repeat(lines.unsigned_abs() as usize))
        } else {
            wheel_as_arrows(lines, &modes)
        };
        if let Some(bytes) = bytes {
            drop(model);
            self.send(bytes, cx);
            return;
        }
        model.scroll_lines(lines);
        drop(model);
        self.show_scrollbar();
        self.sink.check_paging(self.pane);
        cx.notify();
    }
}

fn to_button(button: gpui::MouseButton) -> Option<MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(MouseButton::Left),
        gpui::MouseButton::Middle => Some(MouseButton::Middle),
        gpui::MouseButton::Right => Some(MouseButton::Right),
        gpui::MouseButton::Navigate(_) => None,
    }
}

fn mods(m: Modifiers) -> KeyMods {
    KeyMods {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.control,
    }
}

/// Whether mouse events go to the agent rather than to the selection: a
/// mouse mode is on, the view follows the grid, and Shift is not held.
fn reports(model: &PaneModel, m: Modifiers) -> bool {
    let modes = model.terminal().modes();
    let mouse = modes.mouse_click || modes.mouse_drag || modes.mouse_motion;
    mouse && !m.shift && model.scroll_offset() == 0
}

/// The drawn grid's cell under `pos`, clamped onto it.
fn cell_at(
    layout: Option<GridLayout>,
    model: &PaneModel,
    pos: Point<Pixels>,
) -> Option<(u16, u16)> {
    let l = layout?;
    let (cols, rows) = model.terminal().size();
    let grid = Bounds::new(
        l.origin,
        size(l.cell_w * f32::from(cols), l.line_h * f32::from(rows)),
    );
    Some(point_to_cell(pos, grid, l.cell_w, l.line_h, cols, rows))
}

/// The line and column visible cell `(col, row)` shows.
fn sel_point(model: &PaneModel, (col, row): (u16, u16)) -> SelPoint {
    let line = match model.view_row(row) {
        ViewRow::History(abs) => LineRef::History(abs),
        ViewRow::Grid(r) => LineRef::Grid(r),
    };
    SelPoint { line, col }
}
