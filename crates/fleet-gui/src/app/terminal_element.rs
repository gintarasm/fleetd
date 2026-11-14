//! Draws a pane's grid: cell backgrounds, text, selection, cursor, IME
//! marked text and the "new lines" pill, and routes the pane's mouse events
//! to [`PaneView`]. Built each frame by [`PaneView`].

use std::sync::{Arc, Mutex, MutexGuard};

use fleet_term::{Cell, CursorShape, CursorView};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior,
    Hsla, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, ScrollWheelEvent, SharedString, StrikethroughStyle, Style, TextRun,
    UnderlineStyle, Window, fill, outline, point, px, relative, size,
};

use super::bench;
use super::pane_view::{GridLayout, InputSink, PaneView, grid_size, ime_cell};
use super::theme::Theme;
use crate::core::pane::{PaneModel, ViewRow};
use crate::core::selection::LineRef;
use crate::core::session::PaneId;

/// Cursor bar thickness (beam width, underline height).
const CURSOR_BAR: Pixels = px(2.0);

pub struct TerminalElement {
    pub(crate) view: Entity<PaneView>,
    pub(crate) pane: PaneId,
    pub(crate) model: Arc<Mutex<PaneModel>>,
    pub(crate) sink: Arc<dyn InputSink>,
    pub(crate) focus: FocusHandle,
    pub(crate) theme: Theme,
    pub(crate) font: Font,
    pub(crate) font_size: Pixels,
    /// The pane has keyboard focus in the active window.
    pub(crate) focused: bool,
    pub(crate) marked: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Metrics {
    cell_w: Pixels,
    line_h: Pixels,
}

/// One visible row, copied out of the model.
struct RowSnapshot {
    cells: Vec<Cell>,
    /// The selected columns, `from..to`.
    selected: Option<(u16, u16)>,
}

/// What a frame draws, copied out under one model lock.
struct Snapshot {
    rows: Vec<RowSnapshot>,
    /// `None` when hidden or scrolled into history.
    cursor: Option<CursorView>,
    /// Where IME marked text goes, `(col, row)`.
    ime_at: (u16, u16),
    /// Lines of output below a scrolled-up view (0 when following).
    unseen: u64,
}

fn lock(model: &Mutex<PaneModel>) -> MutexGuard<'_, PaneModel> {
    model.lock().unwrap_or_else(|e| e.into_inner())
}

impl TerminalElement {
    fn snapshot(&self) -> Snapshot {
        let model = lock(&self.model);
        let term = model.terminal();
        let (cols, rows) = term.size();
        let mut view_rows = Vec::with_capacity(usize::from(rows));
        model.view_rows(rows, &mut view_rows);
        let rows = view_rows
            .iter()
            .map(|&vr| {
                let mut cells = Vec::with_capacity(usize::from(cols));
                let line = match vr {
                    ViewRow::History(abs) => {
                        model.history_cells(abs, &mut cells);
                        LineRef::History(abs)
                    }
                    ViewRow::Grid(row) => {
                        term.row_cells(row, &mut cells);
                        LineRef::Grid(row)
                    }
                };
                let selected = model
                    .selected_cols(line)
                    .map(|r| (r.start.min(cols), r.end.min(cols)))
                    .filter(|(from, to)| from < to);
                RowSnapshot { cells, selected }
            })
            .collect();
        let cursor = term.cursor_view();
        let scrolled = model.scroll_offset() > 0;
        let cursor = (!scrolled && cursor.shape != CursorShape::Hidden).then_some(cursor);
        Snapshot {
            rows,
            cursor,
            ime_at: ime_cell(&model),
            unseen: if scrolled { model.unseen_lines() } else { 0 },
        }
    }

    fn paint_row(
        &self,
        row: &RowSnapshot,
        origin: Point<Pixels>,
        m: Metrics,
        block_cursor_col: Option<u16>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let default_bg = self.theme.bg();
        let mut styled: Vec<(u16, u16, CellLook)> = Vec::with_capacity(row.cells.len());
        let mut col = 0u16;
        for cell in &row.cells {
            let width = if cell.wide { 2 } else { 1 };
            let look = self.look(cell, block_cursor_col == Some(col));
            styled.push((col, width, look));
            col = col.saturating_add(width);
        }

        // Backgrounds, merging neighbours of one colour.
        let mut run: Option<(u16, u16, Hsla)> = None;
        for &(col, width, ref look) in &styled {
            match &mut run {
                Some((_, end, bg)) if *end == col && *bg == look.bg => *end += width,
                _ => {
                    if let Some(r) = run.take() {
                        paint_bg(r, default_bg, origin, m, window);
                    }
                    run = Some((col, col + width, look.bg));
                }
            }
        }
        if let Some(r) = run {
            paint_bg(r, default_bg, origin, m, window);
        }

        for piece in row_pieces(&row.cells, &styled) {
            let line = window.text_system().shape_line(
                SharedString::from(piece.text),
                self.font_size,
                &piece.runs,
                piece.grid.then_some(m.cell_w),
            );
            let at = point(origin.x + m.cell_w * f32::from(piece.start_col), origin.y);
            let _ = line.paint(at, m.line_h, window, cx);
        }
    }

    /// Resolved colours and font of a cell.
    fn look(&self, cell: &Cell, block_cursor: bool) -> CellLook {
        let s = &cell.style;
        let mut fg = self.theme.resolve(s.fg, true);
        let mut bg = self.theme.resolve(s.bg, false);
        if s.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if s.dim {
            fg.a *= 0.6;
        }
        if s.hidden {
            fg = bg;
        }
        if block_cursor {
            fg = self.theme.bg();
            bg = self.theme.cursor();
        }
        let mut font = self.font.clone();
        if s.bold {
            font.weight = FontWeight::BOLD;
        }
        if s.italic {
            font.style = FontStyle::Italic;
        }
        CellLook {
            fg,
            bg,
            font,
            underline: s.underline,
            strikeout: s.strikeout,
        }
    }
}

#[derive(Clone, PartialEq)]
struct CellLook {
    fg: Hsla,
    bg: Hsla,
    font: Font,
    underline: bool,
    strikeout: bool,
}

impl CellLook {
    fn run(&self, len: usize) -> TextRun {
        TextRun {
            len,
            font: self.font.clone(),
            color: self.fg,
            background_color: None,
            underline: self.underline.then_some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(self.fg),
                wavy: false,
            }),
            strikethrough: self.strikeout.then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(self.fg),
            }),
        }
    }
}

/// Text for one `shape_line` call, drawn from `start_col`.
struct Piece {
    start_col: u16,
    text: String,
    runs: Vec<TextRun>,
    /// Every glyph's advance forced to the cell width.
    grid: bool,
}

/// Splits a row's text into shaped pieces: one per stretch of
/// single-width cells in one font, every glyph forced to the cell width.
/// Wide and combining cells are shaped alone at their column with natural
/// advances: a forced advance would misplace what follows them (or push a
/// combining mark into the next cell).
///
/// A piece never mixes fonts: gpui 0.2.2's `layout_line` merges a run into
/// the previous font run whenever colour and decorations match, without
/// looking at the run's font, so bold or italic next to a same-coloured
/// run would take that run's font.
fn row_pieces(cells: &[Cell], styled: &[(u16, u16, CellLook)]) -> Vec<Piece> {
    let end = cells
        .iter()
        .rposition(|c| (!c.style.hidden && c.ch != ' ') || c.style.underline || c.style.strikeout)
        .map_or(0, |i| i + 1);
    let mut out = Vec::new();
    let mut seg = Segment::default();
    for (cell, &(col, _, ref look)) in cells[..end].iter().zip(styled) {
        if cell.wide || cell.extra.is_some() {
            seg.flush(true, &mut out);
            let mut alone = Segment::default();
            alone.push(cell, col, look);
            alone.flush(false, &mut out);
        } else {
            if seg.last.as_ref().is_some_and(|l| l.font != look.font) {
                seg.flush(true, &mut out);
            }
            seg.push(cell, col, look);
        }
    }
    seg.flush(true, &mut out);
    out
}

/// Cells collected for one [`Piece`].
#[derive(Default)]
struct Segment {
    start_col: u16,
    text: String,
    runs: Vec<TextRun>,
    last: Option<CellLook>,
}

impl Segment {
    fn push(&mut self, cell: &Cell, col: u16, look: &CellLook) {
        if self.text.is_empty() {
            self.start_col = col;
        }
        let before = self.text.len();
        if cell.style.hidden {
            self.text.push(' ');
        } else {
            self.text.push(cell.ch);
            if let Some(extra) = &cell.extra {
                self.text.push_str(extra);
            }
        }
        let len = self.text.len() - before;
        let same = self.last.as_ref().is_some_and(|l| {
            l.fg == look.fg
                && l.font == look.font
                && l.underline == look.underline
                && l.strikeout == look.strikeout
        });
        match self.runs.last_mut() {
            Some(run) if same => run.len += len,
            _ => {
                self.runs.push(look.run(len));
                self.last = Some(look.clone());
            }
        }
    }

    /// Ends the segment as a piece in `out`; `grid` forces every glyph's
    /// advance to the cell width.
    fn flush(&mut self, grid: bool, out: &mut Vec<Piece>) {
        self.last = None;
        if self.text.is_empty() {
            return;
        }
        out.push(Piece {
            start_col: self.start_col,
            text: std::mem::take(&mut self.text),
            runs: std::mem::take(&mut self.runs),
            grid,
        });
    }
}

fn paint_bg(
    (from, to, bg): (u16, u16, Hsla),
    default_bg: Hsla,
    origin: Point<Pixels>,
    m: Metrics,
    window: &mut Window,
) {
    if bg == default_bg {
        return;
    }
    window.paint_quad(fill(cell_bounds(origin, m, from, to - from), bg));
}

/// Bounds of `width` cells from `col` on the row at `origin`.
fn cell_bounds(origin: Point<Pixels>, m: Metrics, col: u16, width: u16) -> Bounds<Pixels> {
    Bounds::new(
        point(origin.x + m.cell_w * f32::from(col), origin.y),
        size(m.cell_w * f32::from(width), m.line_h),
    )
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = (Metrics, Hitbox);

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> (Metrics, Hitbox) {
        let ts = window.text_system();
        let font_id = ts.resolve_font(&self.font);
        let cell_w = ts
            .advance(font_id, self.font_size, 'm')
            .map_or(self.font_size * 0.6, |a| a.width);
        let line_h =
            (ts.ascent(font_id, self.font_size) + ts.descent(font_id, self.font_size).abs()).ceil();
        let (cols, rows) = grid_size(
            bounds.size.width.into(),
            bounds.size.height.into(),
            cell_w.into(),
            line_h.into(),
        );
        let layout = GridLayout {
            origin: bounds.origin,
            cell_w,
            line_h,
            cols,
            rows,
        };
        let resized = self.view.update(cx, |view, _| {
            let old = view.layout.replace(layout);
            old.is_none_or(|o| (o.cols, o.rows) != (cols, rows))
        });
        if resized {
            self.sink.resize(self.pane, cols, rows);
            // An observer draws at the primary's size (ADR 0022).
            let mut model = lock(&self.model);
            if model.is_primary() {
                model.on_resized(cols, rows);
            }
        }
        // Overlays in front (a sheet, a menu) block it.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        (Metrics { cell_w, line_h }, hitbox)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        (m, hitbox): &mut (Metrics, Hitbox),
        window: &mut Window,
        cx: &mut App,
    ) {
        let m = *m;
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        self.listen_to_mouse(hitbox.clone(), window);
        // The lock is held only for the copy; shaping happens after.
        let snap = self.snapshot();
        if bench::watches(self.pane) {
            let count = snap.rows.iter().map(|r| bench::count_key(&r.cells)).sum();
            bench::painted(self.pane, count, window);
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let block_cursor = snap
                .cursor
                .filter(|c| self.focused && c.shape == CursorShape::Block);
            for (i, row) in snap.rows.iter().enumerate() {
                let origin = point(bounds.origin.x, bounds.origin.y + m.line_h * i as f32);
                let block_col = block_cursor
                    .filter(|c| usize::from(c.row) == i)
                    .map(|c| c.col);
                self.paint_row(row, origin, m, block_col, window, cx);
                if let Some((from, to)) = row.selected {
                    window.paint_quad(fill(
                        cell_bounds(origin, m, from, to - from),
                        self.theme.selection,
                    ));
                }
            }
            if let Some(cursor) = snap.cursor {
                self.paint_cursor(cursor, &snap, bounds.origin, m, window);
            }
            if let Some(marked) = &self.marked {
                self.paint_marked(marked, snap.ime_at, bounds.origin, m, window, cx);
            }
            let pill =
                (snap.unseen > 0).then(|| self.paint_pill(snap.unseen, bounds, m, window, cx));
            self.view.update(cx, |view, _| view.pill = pill);
        });
    }
}

impl Snapshot {
    /// Whether the cell at the cursor is double width.
    fn cursor_is_wide(&self, cursor: CursorView) -> bool {
        let Some(row) = self.rows.get(usize::from(cursor.row)) else {
            return false;
        };
        let mut col = 0u16;
        for cell in &row.cells {
            if col == cursor.col {
                return cell.wide;
            }
            col = col.saturating_add(if cell.wide { 2 } else { 1 });
            if col > cursor.col {
                break;
            }
        }
        false
    }
}

impl TerminalElement {
    fn paint_cursor(
        &self,
        cursor: CursorView,
        snap: &Snapshot,
        origin: Point<Pixels>,
        m: Metrics,
        window: &mut Window,
    ) {
        let width = if snap.cursor_is_wide(cursor) { 2 } else { 1 };
        let row_origin = point(origin.x, origin.y + m.line_h * f32::from(cursor.row));
        let cell = cell_bounds(row_origin, m, cursor.col, width);
        let color = self.theme.cursor();
        if !self.focused {
            window.paint_quad(outline(cell, color, BorderStyle::Solid));
            return;
        }
        match cursor.shape {
            // Drawn with the cell: cursor-coloured background, inverted text.
            CursorShape::Block | CursorShape::Hidden => {}
            CursorShape::Underline => window.paint_quad(fill(
                Bounds::new(
                    point(cell.origin.x, cell.origin.y + cell.size.height - CURSOR_BAR),
                    size(cell.size.width, CURSOR_BAR),
                ),
                color,
            )),
            CursorShape::Beam => window.paint_quad(fill(
                Bounds::new(cell.origin, size(CURSOR_BAR, cell.size.height)),
                color,
            )),
        }
    }

    fn paint_marked(
        &self,
        marked: &str,
        (col, row): (u16, u16),
        origin: Point<Pixels>,
        m: Metrics,
        window: &mut Window,
        cx: &mut App,
    ) {
        let fg = self.theme.fg();
        let run = TextRun {
            len: marked.len(),
            font: self.font.clone(),
            color: fg,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(fg),
                wavy: false,
            }),
            strikethrough: None,
        };
        let line = window.text_system().shape_line(
            SharedString::from(marked.to_owned()),
            self.font_size,
            &[run],
            None,
        );
        let at = point(
            origin.x + m.cell_w * f32::from(col),
            origin.y + m.line_h * f32::from(row),
        );
        window.paint_quad(fill(
            Bounds::new(at, size(line.width.max(m.cell_w), m.line_h)),
            self.theme.marked_bg,
        ));
        let _ = line.paint(at, m.line_h, window, cx);
    }

    /// Paints "↓ N new lines" at the bottom right; returns its bounds.
    fn paint_pill(
        &self,
        unseen: u64,
        bounds: Bounds<Pixels>,
        m: Metrics,
        window: &mut Window,
        cx: &mut App,
    ) -> Bounds<Pixels> {
        let text = format!("↓ {unseen} new line{}", if unseen == 1 { "" } else { "s" });
        let run = TextRun {
            len: text.len(),
            font: self.font.clone(),
            color: self.theme.fg(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line =
            window
                .text_system()
                .shape_line(SharedString::from(text), self.font_size, &[run], None);
        let pad = m.cell_w;
        let pill_size = size(line.width + pad * 2.0, m.line_h * 1.5);
        let pill = Bounds::new(
            point(
                bounds.right() - pill_size.width - pad,
                bounds.bottom() - pill_size.height - m.line_h * 0.5,
            ),
            pill_size,
        );
        window.paint_quad(fill(pill, self.theme.header_bg).corner_radii(pill_size.height / 2.0));
        let text_at = point(pill.origin.x + pad, pill.origin.y + m.line_h * 0.25);
        let _ = line.paint(text_at, m.line_h, window, cx);
        pill
    }

    /// Routes this frame's mouse events to the view: presses and the wheel
    /// over the grid, unless something in front covers it; moves and
    /// releases anywhere, for a drag in progress.
    fn listen_to_mouse(&self, hitbox: Hitbox, window: &mut Window) {
        let view = self.view.clone();
        let down = hitbox.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && down.is_hovered(window) {
                view.update(cx, |v, cx| v.mouse_down(ev, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.mouse_move(ev, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.mouse_up(ev, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                view.update(cx, |v, cx| v.scroll_wheel(ev, window, cx));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use fleet_term::{Cell, CellStyle};
    use gpui::{FontStyle, FontWeight, font, hsla};

    use super::{CellLook, row_pieces};

    fn look(bold: bool, italic: bool) -> CellLook {
        let mut f = font("Menlo");
        if bold {
            f.weight = FontWeight::BOLD;
        }
        if italic {
            f.style = FontStyle::Italic;
        }
        CellLook {
            fg: hsla(0., 0., 1., 1.),
            bg: hsla(0., 0., 0., 1.),
            font: f,
            underline: false,
            strikeout: false,
        }
    }

    /// Cells for `text`, styled per char by `styles` (`b`, `i`, `B` = both, else plain).
    fn row(text: &str, styles: &str) -> (Vec<Cell>, Vec<(u16, u16, CellLook)>) {
        let mut cells = Vec::new();
        let mut styled = Vec::new();
        for (i, (ch, st)) in text.chars().zip(styles.chars()).enumerate() {
            let (bold, italic) = match st {
                'b' => (true, false),
                'i' => (false, true),
                'B' => (true, true),
                _ => (false, false),
            };
            cells.push(Cell {
                ch,
                extra: None,
                style: CellStyle {
                    bold,
                    italic,
                    ..CellStyle::default()
                },
                wide: false,
            });
            styled.push((i as u16, 1, look(bold, italic)));
        }
        (cells, styled)
    }

    #[test]
    fn same_coloured_runs_in_other_fonts_are_shaped_apart() {
        // `\e[3mit\e[0m pl` and `pl \e[1mbo\e[0m`: one colour throughout.
        let (cells, styled) = row("it pl bo BI", "ii....bb.BB");
        let pieces = row_pieces(&cells, &styled);
        let got: Vec<(u16, &str, FontWeight, FontStyle)> = pieces
            .iter()
            .map(|p| {
                assert_eq!(p.runs.len(), 1, "one font per shaped line: {:?}", p.text);
                (
                    p.start_col,
                    p.text.as_str(),
                    p.runs[0].font.weight,
                    p.runs[0].font.style,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "it", FontWeight::NORMAL, FontStyle::Italic),
                (2, " pl ", FontWeight::NORMAL, FontStyle::Normal),
                (6, "bo", FontWeight::BOLD, FontStyle::Normal),
                (8, " ", FontWeight::NORMAL, FontStyle::Normal),
                (9, "BI", FontWeight::BOLD, FontStyle::Italic),
            ]
        );
        assert!(pieces.iter().all(|p| p.grid));
    }
}
