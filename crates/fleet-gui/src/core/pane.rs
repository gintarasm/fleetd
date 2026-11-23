//! One terminal pane: the client grid, its history and the view over them.
//!
//! No I/O: the session layer feeds it attach results, output, role changes
//! and history pages, and takes replies and page requests from it. The grid
//! is a client [`Terminal`]; lines that scroll off it go to [`Scrollback`]
//! numbered as the node numbers them (decisions 0021 and 0022).

use std::ops::Range;

use fleet_proto::{AttachOk, HistoryPage};
use fleet_term::{Cell, HistoryEvent, Palette, Terminal, parse_history_line};

use super::find::{FindStatus, Needle, Search, Step};
use super::scrollback::{Cap, Scrollback};
use super::selection::{self, LINE_END, LineRef, SelPoint, SelUnit, Selection};

/// Lines asked for per `ReadHistory`.
pub const HISTORY_PAGE_LINES: u32 = 1000;
/// The next page is fetched when the view's top is this close to the oldest
/// loaded line.
pub const PREFETCH_LINES: u64 = 200;

/// Size of the grid before the first attach.
const INITIAL_SIZE: (u16, u16) = (80, 24);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneState {
    Attaching,
    Live,
    Exited(i32),
    /// The node no longer has the agent (it was lost, for example in a
    /// daemon restart): attaching failed.
    Lost,
    Disconnected,
}

/// What one visible row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewRow {
    /// History line by absolute number.
    History(u64),
    /// Grid row.
    Grid(u16),
}

/// A keyboard scroll of the view (decision 0082): ⇧PageUp and ⇧PageDown
/// by a page, ⌘↑ and ⌘↓ (⌘Home, ⌘End) to the top and the bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewScroll {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Lines a page scroll moves on a grid `rows` high: all but one, so the
/// line at the edge stays in view for context.
pub fn page_lines(rows: u16) -> u64 {
    u64::from(rows.saturating_sub(1).max(1))
}

pub struct PaneModel {
    palette: Palette,
    term: Terminal,
    scrollback: Scrollback,
    state: PaneState,
    /// The agent of the last applied attach.
    agent: Option<u64>,
    primary: bool,
    page_in_flight: bool,
    /// History lines the view is scrolled up by; 0 follows the grid.
    scroll_offset: u64,
    /// History lines added below the view since it last followed the grid.
    unseen_lines: u64,
    selection: Option<Selection>,
    /// How much history stays loaded.
    history_cap: Cap,
    /// The find bar's search, while it is open.
    find: Option<Search>,
}

impl PaneModel {
    pub fn new(palette: Palette) -> Self {
        Self {
            palette,
            term: Terminal::new_client(INITIAL_SIZE.0, INITIAL_SIZE.1, palette),
            scrollback: Scrollback::default(),
            state: PaneState::Attaching,
            agent: None,
            primary: false,
            page_in_flight: false,
            scroll_offset: 0,
            unseen_lines: 0,
            selection: None,
            history_cap: Cap::default(),
            find: None,
        }
    }

    /// Rebuilds the pane from an attach (or re-attach): a new grid with the
    /// snapshot fed into it, and the history tail as the only loaded history.
    ///
    /// Rows the snapshot pushes off the grid are discarded: the node has
    /// already sent them as the tail. A page request still in flight is
    /// forgotten (its connection is gone).
    pub fn on_attached(&mut self, ok: &AttachOk) {
        let cols = u16::try_from(ok.cols).unwrap_or(u16::MAX);
        let rows = u16::try_from(ok.rows).unwrap_or(u16::MAX);
        self.term = Terminal::new_client(cols, rows, self.palette);
        self.scrollback = Scrollback::with_cap(
            ok.history_generation,
            ok.tail_first_line,
            ok.history_first_line,
            self.history_cap,
        );
        for line in &ok.history_tail {
            self.scrollback.push_back(line.to_vec());
        }
        self.term.feed(&ok.snapshot);
        self.term.drain_history();
        self.term.take_replies();
        self.primary = ok.primary;
        self.agent = Some(ok.agent_id);
        self.state = PaneState::Live;
        self.page_in_flight = false;
        self.scroll_offset = 0;
        self.unseen_lines = 0;
        self.selection = None;
        if let Some(f) = &mut self.find {
            f.restart(&self.scrollback);
        }
    }

    /// Feeds agent output to the grid and moves what scrolled off into
    /// history.
    pub fn on_output(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.term.feed(data);
        self.grid_changed();
    }

    /// Whether the agent rang the bell (BEL) since the last call; clears it.
    pub fn take_bell(&mut self) -> bool {
        self.term.take_bell()
    }

    /// The agent's grid was resized.
    pub fn on_resized(&mut self, cols: u16, rows: u16) {
        self.term.resize(cols, rows);
        self.grid_changed();
    }

    /// The node changed this client's role; only a primary answers queries.
    pub fn on_role(&mut self, primary: bool) {
        self.primary = primary;
    }

    pub fn on_exited(&mut self, code: i32) {
        self.state = PaneState::Exited(code);
    }

    pub fn on_disconnected(&mut self) {
        self.state = PaneState::Disconnected;
    }

    /// An attach (or re-attach) was sent; the grid stays as it is until
    /// [`PaneModel::on_attached`].
    pub fn on_attaching(&mut self) {
        self.state = PaneState::Attaching;
    }

    /// Attaching failed: the node does not have the agent any more.
    pub fn on_lost(&mut self) {
        self.state = PaneState::Lost;
    }

    /// Replaces the palette that answers colour queries, now and after
    /// every later attach.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.term.set_palette(palette);
    }

    /// Replies to terminal queries to send to the agent: empty unless
    /// primary (an observer must not answer).
    pub fn take_replies(&mut self) -> Vec<u8> {
        let replies = self.term.take_replies();
        if self.primary { replies } else { Vec::new() }
    }

    /// `(generation, before_line)` of the next `ReadHistory` to send, once:
    /// when the view's top is within [`PREFETCH_LINES`] of the oldest loaded
    /// line (or an open find has searched down to it), older history
    /// exists, no request is in flight and the loaded history is not at its
    /// cap.
    pub fn next_page_request(&mut self) -> Option<(u64, u64)> {
        let sb = &self.scrollback;
        if self.page_in_flight || sb.fully_loaded() || sb.is_full() {
            return None;
        }
        let top = sb.end_line() - self.scroll_offset;
        let finding = self.find.as_ref().is_some_and(|f| f.at_oldest(sb));
        if top - sb.first_line() > PREFETCH_LINES && !finding {
            return None;
        }
        self.page_in_flight = true;
        Some((sb.generation(), sb.first_line()))
    }

    /// A page answering the request made with `generation`. Dropped if a
    /// clear happened since or it does not join the loaded lines; a page of
    /// the current generation that adds nothing older ends the paging
    /// (unless the cap left its lines out).
    pub fn on_page(&mut self, page: &HistoryPage, generation: u64) {
        self.page_in_flight = false;
        let before = self.scrollback.first_line();
        let lines = page.lines.iter().map(|l| l.to_vec()).collect();
        self.scrollback
            .prepend_page(generation, page.first_line, lines);
        if generation == self.scrollback.generation()
            && self.scrollback.first_line() == before
            && !self.scrollback.is_full()
        {
            self.scrollback.mark_fully_loaded();
        }
    }

    /// The page request failed (for example its generation was stale):
    /// nothing older is fetched in this generation.
    pub fn on_page_error(&mut self) {
        self.page_in_flight = false;
        self.scrollback.mark_fully_loaded();
    }

    /// The request [`PaneModel::next_page_request`] returned was not sent:
    /// it may be asked for again.
    pub fn cancel_page_request(&mut self) {
        self.page_in_flight = false;
    }

    /// Scrolls by `delta` lines, positive up into history, clamped to the
    /// loaded history.
    pub fn scroll_lines(&mut self, delta: i32) {
        let offset = self.scroll_offset.saturating_add_signed(i64::from(delta));
        self.scroll_offset = offset.min(self.scrollback.len());
        if self.scroll_offset == 0 {
            self.unseen_lines = 0;
        }
    }

    /// Scrolls up by `offset` lines (the dragged scrollbar), clamped to
    /// the loaded history.
    pub fn scroll_to(&mut self, offset: u64) {
        self.scroll_offset = offset.min(self.scrollback.len());
        if self.scroll_offset == 0 {
            self.unseen_lines = 0;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.unseen_lines = 0;
    }

    /// Scrolls the view from the keyboard. `false` on the alternate screen,
    /// which has no history: the key is the program's then.
    pub fn scroll_view(&mut self, scroll: ViewScroll) -> bool {
        if self.term.modes().alt_screen {
            return false;
        }
        let page = page_lines(self.term.size().1);
        match scroll {
            ViewScroll::PageUp => self.scroll_to(self.scroll_offset.saturating_add(page)),
            ViewScroll::PageDown => self.scroll_to(self.scroll_offset.saturating_sub(page)),
            ViewScroll::Top => self.scroll_to(self.scrollback.len()),
            ViewScroll::Bottom => self.scroll_to_bottom(),
        }
        true
    }

    /// Keyboard scrolling applies (not the alternate screen) and there is
    /// history to scroll through.
    pub fn can_scroll_view(&self) -> bool {
        !self.term.modes().alt_screen && !self.scrollback.is_empty()
    }

    /// Selects the loaded history and the grid down to its last row with
    /// text (⌘A); `false`, selecting nothing, when there is no text.
    pub fn select_all(&mut self) -> bool {
        let mut cells = Vec::new();
        let rows = self.term.size().1;
        let last_row = (0..rows).rev().find(|&r| {
            self.term.row_cells(r, &mut cells);
            !selection::row_text(&cells, 0, LINE_END).is_empty()
        });
        let sb = &self.scrollback;
        let end = match last_row {
            Some(r) => LineRef::Grid(r),
            None if !sb.is_empty() => LineRef::History(sb.end_line() - 1),
            None => {
                self.selection = None;
                return false;
            }
        };
        let start = if sb.is_empty() {
            LineRef::Grid(0)
        } else {
            LineRef::History(sb.first_line())
        };
        self.selection = Some(Selection::range(
            SelUnit::Line,
            SelPoint {
                line: start,
                col: 0,
            },
            SelPoint {
                line: end,
                col: LINE_END,
            },
        ));
        true
    }

    /// Clears the loaded history from this view only (⌘K): the agent and
    /// the node's history are untouched, and a re-attach brings it back.
    /// The view returns to the grid; a selection in the history is dropped
    /// and an open find searches again.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear_view();
        self.scroll_offset = 0;
        self.unseen_lines = 0;
        if self
            .selection
            .is_some_and(|s| s.touches_history_before(self.scrollback.first_line()))
        {
            self.selection = None;
        }
        if let Some(f) = &mut self.find {
            f.restart(&self.scrollback);
        }
    }

    /// Lines of output that scrolled into history below the view while it
    /// was scrolled up; 0 once it follows the grid again.
    pub fn unseen_lines(&self) -> u64 {
        self.unseen_lines
    }

    /// The "New output" chip shows: the view is scrolled up and output has
    /// scrolled lines into history below it since. Redraws in place (a
    /// spinner, a status line) add no lines and show no chip.
    pub fn new_output(&self) -> bool {
        self.scroll_offset > 0 && self.unseen_lines > 0
    }

    /// History lines the view is scrolled up by; 0 follows the live grid.
    pub fn scroll_offset(&self) -> u64 {
        self.scroll_offset
    }

    /// Replaces `out` with what each of `rows` visible rows shows, top to
    /// bottom.
    pub fn view_rows(&self, rows: u16, out: &mut Vec<ViewRow>) {
        out.clear();
        out.extend((0..rows).map(|i| self.view_row(i)));
    }

    /// What visible row `row` (from the top) shows.
    pub fn view_row(&self, row: u16) -> ViewRow {
        let abs = self.scrollback.end_line() - self.scroll_offset + u64::from(row);
        match abs.checked_sub(self.scrollback.end_line()) {
            Some(row) => ViewRow::Grid(row as u16),
            None => ViewRow::History(abs),
        }
    }

    /// Replaces `out` with the cells of history line `abs` (empty if not
    /// loaded).
    pub fn history_cells(&self, abs: u64, out: &mut Vec<Cell>) {
        parse_history_line(self.scrollback.line(abs).unwrap_or_default(), out);
    }

    /// The client grid (cursor, modes and rows for drawing and input).
    pub fn terminal(&self) -> &Terminal {
        &self.term
    }

    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    pub fn title(&self) -> Option<String> {
        self.term.title()
    }

    /// The agent the grid shows (of the last applied attach, which for a
    /// spawn is the new agent).
    pub fn agent_id(&self) -> Option<u64> {
        self.agent
    }

    pub fn state(&self) -> PaneState {
        self.state
    }

    pub fn is_primary(&self) -> bool {
        self.primary
    }

    /// Starts a cell selection at `p` (empty until extended).
    pub fn select_start(&mut self, p: SelPoint) {
        self.selection = Some(Selection::cells(p));
    }

    /// Moves the selection's moving end to `p`, snapping to words or lines
    /// when the selection started as one.
    pub fn select_extend(&mut self, p: SelPoint) {
        let Some(unit) = self.selection.map(|s| s.unit()) else {
            return;
        };
        let range = match unit {
            SelUnit::Cell => None,
            SelUnit::Word => Some(self.word_range(p)),
            SelUnit::Line => Some(line_range(p)),
        };
        if let Some(sel) = &mut self.selection {
            match range {
                None => sel.extend_cells(p),
                Some((start, end)) => sel.extend_range(start, end),
            }
        }
    }

    /// A drag-select past the grid's edge (decision 0095): scrolls the view
    /// by `lines` (positive up) and extends the selection to column `col`
    /// of the row now at that edge, the top going up, the bottom going
    /// down. `false`, doing nothing, when the view cannot move: no
    /// selection, the alternate screen, or already at that end.
    pub fn drag_scroll(&mut self, lines: i32, col: u16) -> bool {
        if lines == 0 || self.selection.is_none() || !self.can_scroll_view() {
            return false;
        }
        let before = self.scroll_offset;
        self.scroll_lines(lines);
        if self.scroll_offset == before {
            return false;
        }
        let row = if lines > 0 {
            0
        } else {
            self.term.size().1.saturating_sub(1)
        };
        let line = match self.view_row(row) {
            ViewRow::History(abs) => LineRef::History(abs),
            ViewRow::Grid(r) => LineRef::Grid(r),
        };
        self.select_extend(SelPoint { line, col });
        true
    }

    /// Selects the word around `p`.
    pub fn select_word(&mut self, p: SelPoint) {
        let (start, end) = self.word_range(p);
        self.selection = Some(Selection::range(SelUnit::Word, start, end));
    }

    /// Selects the whole line of `p`.
    pub fn select_line(&mut self, p: SelPoint) {
        let (start, end) = line_range(p);
        self.selection = Some(Selection::range(SelUnit::Line, start, end));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Some text is selected.
    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// The selected text: one row per line, trailing spaces trimmed, joined
    /// by `\n`.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection?.bounds()?;
        let mut cells = Vec::new();
        let mut rows = Vec::new();
        let mut line = Some(start.line);
        while let Some(at) = line.filter(|&at| at <= end.line) {
            let from = if at == start.line { start.col } else { 0 };
            let to = if at == end.line { end.col } else { LINE_END };
            self.line_cells(at, &mut cells);
            rows.push(selection::row_text(&cells, from, to));
            line = self.next_line(at);
        }
        Some(rows.join("\n"))
    }

    pub fn is_selected(&self, line: LineRef, col: u16) -> bool {
        self.selection.is_some_and(|s| s.contains(line, col))
    }

    /// The selected columns of `line` (`end` exclusive, up to
    /// [`LINE_END`] for "to the end of the line"), `None` if none are.
    pub fn selected_cols(&self, line: LineRef) -> Option<Range<u16>> {
        let (start, end) = self.selection?.bounds()?;
        if line < start.line || line > end.line {
            return None;
        }
        let from = if line == start.line { start.col } else { 0 };
        let to = if line == end.line {
            end.col.saturating_add(1)
        } else {
            LINE_END
        };
        (from < to).then_some(from..to)
    }

    /// Opens find (or changes its query): searches the loaded history and
    /// the grid for `query`, from the newest line, in [`PaneModel::find_step`]s.
    pub fn find_open(&mut self, query: &str, case_sensitive: bool) {
        let needle = Needle::new(query, case_sensitive);
        if self
            .find
            .as_ref()
            .is_some_and(|f| f.needle() == needle.as_ref())
        {
            return;
        }
        self.find = Some(Search::new(needle, &self.scrollback));
    }

    /// Opens find for `query` on the match on `line` (numbered as the
    /// search numbers lines), or the next one below it: Search All Agents
    /// (ADR 0098). The line may be in history not loaded yet; the search
    /// pages it in.
    pub fn find_open_at(&mut self, query: &str, case_sensitive: bool, line: u64) {
        let needle = Needle::new(query, case_sensitive);
        self.find = Some(Search::new_at(needle, &self.scrollback, Some(line)));
    }

    pub fn find_close(&mut self) {
        self.find = None;
    }

    /// Find is open and has lines left to scan.
    pub fn find_pending(&self) -> bool {
        self.find
            .as_ref()
            .is_some_and(|f| !f.done(&self.scrollback))
    }

    /// Scans up to `budget` more lines for the open find. The first match
    /// picked (the newest at or above the view's bottom) is scrolled into
    /// view.
    pub fn find_step(&mut self, budget: usize) -> Step {
        let (_, bottom) = self.view_lines();
        let Some(f) = &mut self.find else {
            return Step::default();
        };
        let before = f.current();
        let step = f.step(&self.scrollback, &self.term, budget, bottom);
        if let Some(m) = f.current().filter(|&m| Some(m) != before) {
            self.reveal(m.line);
        }
        step
    }

    /// Goes to the next match up (`up`) or down, wrapping, and scrolls it
    /// into view; `false` if there is none.
    pub fn find_next(&mut self, up: bool) -> bool {
        let (top, bottom) = self.view_lines();
        let Some(f) = &mut self.find else {
            return false;
        };
        // Matches on a grid that changed since the last step.
        f.step(&self.scrollback, &self.term, 0, bottom);
        match f.advance(up, top, bottom) {
            Some(m) => {
                self.reveal(m.line);
                true
            }
            None => false,
        }
    }

    /// The find bar's match count and place; `None` while find is closed.
    pub fn find_status(&self) -> Option<FindStatus> {
        let f = self.find.as_ref()?;
        Some(FindStatus {
            index: f.current_index().map(|i| i + 1),
            count: f.count(),
            searching: !f.done(&self.scrollback),
        })
    }

    /// Replaces `out` with the matches on `line`: columns `start..end` and
    /// whether it is the current match.
    pub fn find_matches(&self, line: LineRef, out: &mut Vec<(u16, u16, bool)>) {
        out.clear();
        if let Some(f) = &self.find {
            let n = match line {
                LineRef::History(abs) => abs,
                LineRef::Grid(row) => self.scrollback.end_line() + u64::from(row),
            };
            f.on_line(n, out);
        }
    }

    /// The first and last line the view shows, numbered as the search
    /// numbers them (grid row `r` is `end_line + r`).
    fn view_lines(&self) -> (u64, u64) {
        let top = self.scrollback.end_line() - self.scroll_offset;
        let rows = u64::from(self.term.size().1.max(1));
        (top, top + rows - 1)
    }

    /// Scrolls so that `line` (numbered as in [`PaneModel::view_lines`]) is
    /// in view, near the middle, unless it already is.
    fn reveal(&mut self, line: u64) {
        let (top, bottom) = self.view_lines();
        if (top..=bottom).contains(&line) {
            return;
        }
        let end = self.scrollback.end_line();
        self.scroll_offset = if line >= end {
            0
        } else {
            let half = u64::from(self.term.size().1) / 2;
            end.saturating_sub(line.saturating_sub(half))
                .min(self.scrollback.len())
        };
        self.unseen_lines = self.unseen_lines.min(self.scroll_offset);
    }

    /// After the grid changed: history events into the scrollback, the view
    /// pinned, a selection on the grid dropped, observer replies discarded.
    fn grid_changed(&mut self) {
        if let Some(f) = &mut self.find {
            f.grid_changed();
        }
        for event in self.term.drain_history() {
            match event {
                HistoryEvent::Line(line) => {
                    let dropped = self.scrollback.push_back(line);
                    if self.scroll_offset > 0 {
                        self.scroll_offset += 1;
                        self.unseen_lines += 1;
                    }
                    if dropped > 0 {
                        // The view stops at the oldest line still loaded.
                        self.scroll_offset = self.scroll_offset.min(self.scrollback.len());
                        self.unseen_lines = self.unseen_lines.min(self.scroll_offset);
                        let first = self.scrollback.first_line();
                        if self
                            .selection
                            .is_some_and(|s| s.touches_history_before(first))
                        {
                            self.selection = None;
                        }
                    }
                }
                HistoryEvent::Cleared => {
                    self.scrollback.clear();
                    self.scroll_offset = 0;
                    self.unseen_lines = 0;
                    let first = self.scrollback.first_line();
                    if self
                        .selection
                        .is_some_and(|s| s.touches_history_before(first))
                    {
                        self.selection = None;
                    }
                }
            }
        }
        if self.selection.is_some_and(|s| s.touches_grid()) {
            self.selection = None;
        }
        if !self.primary {
            self.term.take_replies();
        }
    }

    fn line_cells(&self, line: LineRef, out: &mut Vec<Cell>) {
        match line {
            LineRef::History(abs) => self.history_cells(abs, out),
            LineRef::Grid(row) => self.term.row_cells(row, out),
        }
    }

    /// The line below `line`, `None` past the last grid row.
    fn next_line(&self, line: LineRef) -> Option<LineRef> {
        match line {
            LineRef::History(abs) if abs + 1 < self.scrollback.end_line() => {
                Some(LineRef::History(abs + 1))
            }
            LineRef::History(_) => Some(LineRef::Grid(0)),
            LineRef::Grid(row) => {
                let rows = self.term.size().1;
                (row + 1 < rows).then_some(LineRef::Grid(row + 1))
            }
        }
    }

    fn word_range(&self, p: SelPoint) -> (SelPoint, SelPoint) {
        let mut cells = Vec::new();
        self.line_cells(p.line, &mut cells);
        let (from, to) = selection::word_bounds(&cells, p.col);
        (
            SelPoint {
                line: p.line,
                col: from,
            },
            SelPoint {
                line: p.line,
                col: to,
            },
        )
    }
}

fn line_range(p: SelPoint) -> (SelPoint, SelPoint) {
    (
        SelPoint {
            line: p.line,
            col: 0,
        },
        SelPoint {
            line: p.line,
            col: LINE_END,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::{AttachOk, HistoryPage};
    use fleet_term::cells_text;

    fn attach_ok(cols: u32, rows: u32, tail: &[&str], tail_first_line: u64) -> AttachOk {
        AttachOk {
            agent_id: 1,
            snapshot: Vec::new(),
            cols,
            rows,
            history_tail: tail.iter().map(|s| s.as_bytes().to_vec().into()).collect(),
            tail_first_line,
            history_first_line: 0,
            history_generation: 0,
            primary: true,
        }
    }

    #[test]
    fn take_bell_reports_a_bel_in_the_output_once() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 5, &[], 0));
        assert!(!pane.take_bell());
        pane.on_output(b"waiting\x07");
        assert!(pane.take_bell());
        assert!(!pane.take_bell());
    }

    fn history_texts(pane: &PaneModel) -> Vec<String> {
        let sb = pane.scrollback();
        let mut cells = Vec::new();
        (sb.first_line()..sb.end_line())
            .map(|n| {
                pane.history_cells(n, &mut cells);
                cells_text(&cells)
            })
            .collect()
    }

    fn pane() -> PaneModel {
        PaneModel::new(Palette {
            fg: (255, 255, 255),
            bg: (0, 0, 0),
            cursor: (255, 255, 255),
            ansi: [(128, 128, 128); 16],
        })
    }

    #[test]
    fn attach_numbers_tail_and_continues_numbering() {
        let mut pane = pane();
        assert_eq!(pane.state(), PaneState::Attaching);
        assert_eq!(pane.agent_id(), None);
        pane.on_attached(&attach_ok(10, 3, &["t0", "t1", "t2"], 100));
        assert_eq!(pane.state(), PaneState::Live);
        assert_eq!(pane.agent_id(), Some(1));
        pane.on_output(b"1\r\n2\r\n3\r\n4\r\n5");
        let sb = pane.scrollback();
        assert_eq!((sb.first_line(), sb.end_line()), (100, 105));
        assert_eq!(history_texts(&pane), ["t0", "t1", "t2", "1", "2"]);
    }

    #[test]
    fn reattach_replaces_history_without_duplicates() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 2, &["a", "b", "c"], 100));
        pane.on_output(b"d\r\ne\r\nf");
        assert_eq!(history_texts(&pane), ["a", "b", "c", "d"]);
        pane.on_attached(&attach_ok(10, 2, &["b", "c", "d", "e"], 101));
        let sb = pane.scrollback();
        assert_eq!((sb.first_line(), sb.end_line()), (101, 105));
        assert_eq!(history_texts(&pane), ["b", "c", "d", "e"]);
    }

    #[test]
    fn attach_discards_history_from_the_snapshot() {
        let mut node = fleet_term::Terminal::new(10, 2);
        node.feed(b"x\r\ny\r\nz");
        let mut ok = attach_ok(10, 2, &["x"], 7);
        ok.snapshot = node.snapshot();
        let mut pane = pane();
        pane.on_attached(&ok);
        assert_eq!(history_texts(&pane), ["x"]);
        assert_eq!(pane.scrollback().end_line(), 8);
    }

    #[test]
    fn page_arriving_after_clear_is_dropped() {
        let mut pane = pane();
        let mut ok = attach_ok(10, 2, &["c", "d"], 2);
        ok.history_first_line = 0;
        pane.on_attached(&ok);
        assert_eq!(pane.next_page_request(), Some((0, 2)));
        pane.on_output(b"\x1b[3J");
        let page = HistoryPage {
            agent_id: 1,
            first_line: 0,
            lines: vec![b"a".to_vec().into(), b"b".to_vec().into()],
        };
        pane.on_page(&page, 0);
        let sb = pane.scrollback();
        assert_eq!(sb.generation(), 1);
        assert_eq!((sb.first_line(), sb.end_line()), (4, 4));
        assert_eq!(pane.next_page_request(), None);
    }

    #[test]
    fn clear_split_across_outputs_is_honoured() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 2, &["a", "b"], 0));
        pane.on_output(b"\x1b[3");
        assert_eq!(pane.scrollback().generation(), 0);
        pane.on_output(b"J");
        let sb = pane.scrollback();
        assert_eq!(sb.generation(), 1);
        assert_eq!((sb.first_line(), sb.end_line()), (2, 2));
    }

    #[test]
    fn next_page_request_fires_once_until_answered() {
        let mut pane = pane();
        let mut ok = attach_ok(10, 2, &["e", "f"], 3000);
        ok.history_first_line = 0;
        ok.history_generation = 7;
        pane.on_attached(&ok);
        assert_eq!(pane.next_page_request(), Some((7, 3000)));
        assert_eq!(pane.next_page_request(), None);
        let page = HistoryPage {
            agent_id: 1,
            first_line: 2000,
            lines: (0..1000).map(|_| b"p".to_vec().into()).collect(),
        };
        pane.on_page(&page, 7);
        assert_eq!(pane.scrollback().first_line(), 2000);
        // The view (at the bottom) is 1002 lines above the oldest: no prefetch.
        assert_eq!(pane.next_page_request(), None);
        pane.scroll_lines(850);
        assert_eq!(pane.next_page_request(), Some((7, 2000)));
        pane.on_page_error();
        assert!(pane.scrollback().fully_loaded());
        assert_eq!(pane.next_page_request(), None);
    }

    #[test]
    fn empty_page_marks_history_fully_loaded() {
        let mut pane = pane();
        let mut ok = attach_ok(10, 2, &["e"], 50);
        ok.history_first_line = 0;
        pane.on_attached(&ok);
        assert_eq!(pane.next_page_request(), Some((0, 50)));
        let page = HistoryPage {
            agent_id: 1,
            first_line: 50,
            lines: Vec::new(),
        };
        pane.on_page(&page, 0);
        assert_eq!(pane.next_page_request(), None);
    }

    #[test]
    fn scrolled_up_view_stays_on_same_content_under_output() {
        let mut pane = pane();
        let tail: Vec<String> = (0..20).map(|i| format!("h{i}")).collect();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        pane.on_attached(&attach_ok(10, 3, &tail, 0));
        pane.on_output(b"\x1b[3;1H");
        pane.scroll_lines(10);
        assert_eq!(pane.scroll_offset(), 10);
        let mut rows = Vec::new();
        pane.view_rows(3, &mut rows);
        assert_eq!(
            rows,
            [
                ViewRow::History(10),
                ViewRow::History(11),
                ViewRow::History(12)
            ]
        );
        pane.on_output(b"\r\n1\r\n2\r\n3\r\n4\r\n5");
        assert_eq!(pane.scroll_offset(), 15);
        let mut after = Vec::new();
        pane.view_rows(3, &mut after);
        assert_eq!(after, rows);
        pane.scroll_lines(-1000);
        assert_eq!(pane.scroll_offset(), 0);
        pane.view_rows(3, &mut rows);
        assert_eq!(rows, [ViewRow::Grid(0), ViewRow::Grid(1), ViewRow::Grid(2)]);
        pane.scroll_lines(1);
        pane.view_rows(3, &mut rows);
        assert_eq!(
            rows,
            [ViewRow::History(24), ViewRow::Grid(0), ViewRow::Grid(1)]
        );
        pane.scroll_lines(1000);
        assert_eq!(pane.scroll_offset(), 25);
        pane.scroll_to_bottom();
        assert_eq!(pane.scroll_offset(), 0);
        // At the bottom, output keeps following.
        pane.on_output(b"\r\n6");
        assert_eq!(pane.scroll_offset(), 0);
    }

    #[test]
    fn output_below_a_scrolled_up_view_counts_unseen_lines() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 3, &["a", "b", "c"], 0));
        pane.on_output(b"\x1b[3;1H");
        pane.on_output(b"\r\n1");
        assert_eq!(pane.unseen_lines(), 0);
        pane.scroll_lines(2);
        pane.on_output(b"\r\n2\r\n3");
        assert_eq!(pane.unseen_lines(), 2);
        pane.scroll_lines(-1);
        assert_eq!(pane.unseen_lines(), 2);
        pane.scroll_lines(-100);
        assert_eq!(pane.unseen_lines(), 0);
        pane.scroll_lines(1);
        pane.on_output(b"\r\n4");
        assert_eq!(pane.unseen_lines(), 1);
        pane.scroll_to_bottom();
        assert_eq!(pane.unseen_lines(), 0);
    }

    #[test]
    fn observer_discards_replies_primary_returns_them() {
        let mut pane = pane();
        let mut ok = attach_ok(10, 3, &[], 0);
        ok.primary = false;
        pane.on_attached(&ok);
        pane.on_output(b"\x1b[6n");
        assert!(pane.take_replies().is_empty());
        pane.on_output(b"\x1b[6n");
        pane.on_role(true);
        pane.on_output(b"ab\x1b[6n");
        assert_eq!(pane.take_replies(), b"\x1b[1;3R");
        assert!(pane.take_replies().is_empty());
    }

    #[test]
    fn selection_across_history_and_grid_copies_text() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 3, &["zero", "hello world  ", "two"], 100));
        pane.on_output(b"grid one\r\ngrid two");
        pane.select_start(SelPoint {
            line: LineRef::History(101),
            col: 2,
        });
        pane.select_extend(SelPoint {
            line: LineRef::Grid(0),
            col: 3,
        });
        assert!(pane.is_selected(LineRef::History(102), 0));
        assert!(!pane.is_selected(LineRef::History(101), 1));
        assert_eq!(pane.selected_cols(LineRef::History(100)), None);
        assert_eq!(pane.selected_cols(LineRef::History(101)), Some(2..LINE_END));
        assert_eq!(pane.selected_cols(LineRef::History(102)), Some(0..LINE_END));
        assert_eq!(pane.selected_cols(LineRef::Grid(0)), Some(0..4));
        assert_eq!(pane.selected_cols(LineRef::Grid(1)), None);
        assert_eq!(
            pane.selection_text().as_deref(),
            Some("llo world\ntwo\ngrid")
        );
    }

    #[test]
    fn word_and_line_selection() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(30, 3, &["cargo test -p fleet-gui"], 0));
        pane.on_output(b"one two three");
        pane.select_word(SelPoint {
            line: LineRef::History(0),
            col: 16,
        });
        assert_eq!(pane.selection_text().as_deref(), Some("fleet-gui"));
        pane.select_extend(SelPoint {
            line: LineRef::Grid(0),
            col: 5,
        });
        assert_eq!(pane.selection_text().as_deref(), Some("fleet-gui\none two"));
        pane.select_line(SelPoint {
            line: LineRef::Grid(0),
            col: 5,
        });
        assert_eq!(pane.selection_text().as_deref(), Some("one two three"));
        pane.clear_selection();
        assert_eq!(pane.selection_text(), None);
    }

    #[test]
    fn output_clears_selection_touching_grid_only() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 2, &["a", "b"], 0));
        pane.select_line(SelPoint {
            line: LineRef::Grid(0),
            col: 0,
        });
        pane.on_output(b"x");
        assert_eq!(pane.selection_text(), None);
        pane.select_line(SelPoint {
            line: LineRef::History(0),
            col: 0,
        });
        pane.on_output(b"y");
        assert_eq!(pane.selection_text().as_deref(), Some("a"));
    }

    #[test]
    fn lifecycle_states_and_title() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 2, &[], 0));
        pane.on_output(b"\x1b]2;build\x07");
        assert_eq!(pane.title().as_deref(), Some("build"));
        pane.on_exited(3);
        assert_eq!(pane.state(), PaneState::Exited(3));
        pane.on_disconnected();
        assert_eq!(pane.state(), PaneState::Disconnected);
    }

    fn capped(lines: usize) -> PaneModel {
        let mut pane = pane();
        pane.history_cap = Cap {
            lines,
            bytes: usize::MAX,
        };
        pane
    }

    #[test]
    fn history_over_the_cap_keeps_the_view_and_drops_a_selection_on_dropped_lines() {
        let mut pane = capped(4);
        let mut ok = attach_ok(10, 2, &["a", "b", "c"], 10);
        ok.history_first_line = 0;
        pane.on_attached(&ok);
        pane.on_output(b"\x1b[2;1H");
        pane.select_line(SelPoint {
            line: LineRef::History(10),
            col: 0,
        });
        pane.scroll_lines(3);
        pane.on_output(b"\r\n1\r\n2\r\n3");
        let sb = pane.scrollback();
        assert_eq!((sb.first_line(), sb.end_line()), (12, 16));
        assert_eq!(history_texts(&pane), ["c", "", "", "1"]);
        // Scrolled to the oldest loaded line, not past it.
        assert_eq!(pane.scroll_offset(), 4);
        let mut rows = Vec::new();
        pane.view_rows(2, &mut rows);
        assert_eq!(rows, [ViewRow::History(12), ViewRow::History(13)]);
        // The selected line 10 is gone.
        assert_eq!(pane.selection_text(), None);
        // The node still has lines 0..12, but the full history pages no more.
        assert!(!pane.scrollback().fully_loaded());
        assert_eq!(pane.next_page_request(), None);
    }

    #[test]
    fn page_cut_by_the_cap_does_not_end_paging_for_good() {
        let mut pane = capped(3);
        let mut ok = attach_ok(10, 2, &["e", "f"], 50);
        ok.history_first_line = 0;
        pane.on_attached(&ok);
        assert_eq!(pane.next_page_request(), Some((0, 50)));
        let page = HistoryPage {
            agent_id: 1,
            first_line: 48,
            lines: vec![b"c".to_vec().into(), b"d".to_vec().into()],
        };
        pane.on_page(&page, 0);
        assert_eq!(history_texts(&pane), ["d", "e", "f"]);
        assert!(!pane.scrollback().fully_loaded());
        assert_eq!(pane.next_page_request(), None);
        // A re-attach rebuilds the history: paging resumes.
        pane.on_attached(&ok);
        assert_eq!(pane.next_page_request(), Some((0, 50)));
    }

    #[test]
    fn resize_moves_rows_pushed_off_into_history() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 3, &[], 40));
        pane.on_output(b"1\r\n2\r\n3");
        pane.on_resized(10, 2);
        assert_eq!(history_texts(&pane), ["1"]);
        assert_eq!(pane.scrollback().first_line(), 40);
    }

    fn find_all(pane: &mut PaneModel, query: &str, case: bool) {
        pane.find_open(query, case);
        while pane.find_step(1000).more {}
    }

    fn found_on(pane: &PaneModel, line: LineRef) -> Vec<(u16, u16, bool)> {
        let mut out = Vec::new();
        pane.find_matches(line, &mut out);
        out
    }

    #[test]
    fn find_matches_history_and_grid_and_starts_on_the_newest() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 2, &["error one", "fine", "an error"], 10));
        pane.on_output(b"Error three");
        find_all(&mut pane, "error", false);
        let status = pane.find_status().unwrap();
        assert_eq!(
            (status.index, status.count, status.searching),
            (Some(3), 3, false)
        );
        assert_eq!(found_on(&pane, LineRef::History(10)), [(0, 5, false)]);
        assert_eq!(found_on(&pane, LineRef::History(11)), []);
        assert_eq!(found_on(&pane, LineRef::History(12)), [(3, 8, false)]);
        assert_eq!(found_on(&pane, LineRef::Grid(0)), [(0, 5, true)]);
        // Case sensitive: only the lower-case ones.
        find_all(&mut pane, "error", true);
        assert_eq!(pane.find_status().unwrap().count, 2);
        assert_eq!(found_on(&pane, LineRef::Grid(0)), []);
        // An empty query finds nothing.
        find_all(&mut pane, "", false);
        let status = pane.find_status().unwrap();
        assert_eq!((status.index, status.count), (None, 0));
    }

    #[test]
    fn find_at_a_line_picks_the_match_there_after_the_attach() {
        let mut pane = pane();
        // Opened before the agent's history arrives (Search All Agents).
        pane.find_open_at("x", false, 1);
        while pane.find_step(1000).more {}
        pane.on_attached(&attach_ok(20, 2, &["x 0", "x 1", "y", "x 3"], 0));
        while pane.find_step(1000).more {}
        assert_eq!(pane.find_status().unwrap().index, Some(2));
        assert_eq!(found_on(&pane, LineRef::History(1)), [(0, 1, true)]);
        // No match on the line itself: the next one below it.
        pane.find_open_at("x", false, 2);
        while pane.find_step(1000).more {}
        assert_eq!(found_on(&pane, LineRef::History(3)), [(0, 1, true)]);
        // On the grid: numbered past the history's end.
        pane.on_output(b"x grid");
        pane.find_open_at("x", false, 4);
        while pane.find_step(1000).more {}
        assert_eq!(found_on(&pane, LineRef::Grid(0)), [(0, 1, true)]);
        // Past every match: the last one.
        pane.find_open_at("x", false, 99);
        while pane.find_step(1000).more {}
        assert_eq!(found_on(&pane, LineRef::Grid(0)), [(0, 1, true)]);
    }

    #[test]
    fn find_at_an_older_line_waits_for_its_page() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 2, &["x 50", "x 51"], 50));
        pane.find_open_at("x", false, 10);
        while pane.find_step(1000).more {}
        assert_eq!(
            pane.find_status().unwrap().index,
            None,
            "line 10 is not loaded yet"
        );
        assert_eq!(pane.next_page_request(), Some((0, 50)));
        let lines: Vec<_> = (0..50)
            .map(|i| format!("x {i}").into_bytes().into())
            .collect();
        pane.on_page(
            &HistoryPage {
                agent_id: 1,
                first_line: 0,
                lines,
            },
            0,
        );
        while pane.find_step(1000).more {}
        assert_eq!(found_on(&pane, LineRef::History(10)), [(0, 1, true)]);
    }

    #[test]
    fn find_next_goes_up_and_wraps_previous_goes_down() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 2, &["x 1", "x 2", "y", "x 3"], 0));
        find_all(&mut pane, "x", false);
        let index = |p: &PaneModel| p.find_status().unwrap().index;
        assert_eq!(index(&pane), Some(3));
        assert!(pane.find_next(true));
        assert_eq!(index(&pane), Some(2));
        assert!(pane.find_next(true));
        assert!(pane.find_next(true));
        assert_eq!(index(&pane), Some(3));
        assert!(pane.find_next(false));
        assert_eq!(index(&pane), Some(1));
        assert_eq!(found_on(&pane, LineRef::History(0)), [(0, 1, true)]);
    }

    #[test]
    fn the_current_match_is_scrolled_into_view() {
        let mut pane = pane();
        let mut tail: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        tail[10] = "needle".into();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        pane.on_attached(&attach_ok(20, 3, &tail, 0));
        assert_eq!(pane.scroll_offset(), 0);
        find_all(&mut pane, "needle", false);
        let mut rows = Vec::new();
        pane.view_rows(3, &mut rows);
        assert!(rows.contains(&ViewRow::History(10)), "{rows:?}");
        // Already in view: going to it again does not scroll.
        let offset = pane.scroll_offset();
        assert!(pane.find_next(true));
        assert_eq!(pane.scroll_offset(), offset);
        // A match on the grid brings the view back down.
        pane.on_output(b"needle");
        while pane.find_step(1000).more {}
        assert!(pane.find_next(false));
        assert_eq!(pane.scroll_offset(), 0);
    }

    #[test]
    fn find_scans_in_bounded_steps() {
        let mut pane = pane();
        let tail: Vec<String> = (0..1000).map(|i| format!("hit {i}")).collect();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        pane.on_attached(&attach_ok(20, 2, &tail, 0));
        pane.find_open("hit", false);
        let step = pane.find_step(100);
        assert!(step.more && step.changed);
        let status = pane.find_status().unwrap();
        assert!(status.searching);
        assert!(status.count < 100, "{}", status.count);
        // The newest is picked once it is certain.
        assert_eq!(status.index, Some(status.count));
        let mut steps = 1;
        while pane.find_step(100).more {
            steps += 1;
        }
        assert!(steps >= 10, "{steps}");
        let status = pane.find_status().unwrap();
        assert_eq!(
            (status.count, status.index, status.searching),
            (1000, Some(1000), false)
        );
    }

    #[test]
    fn find_follows_new_output_and_keeps_the_current_match_as_it_scrolls() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 2, &["old hit"], 0));
        pane.on_output(b"new hit");
        find_all(&mut pane, "hit", false);
        assert_eq!(pane.find_status().unwrap().index, Some(2));
        // The grid row with the current match scrolls into history.
        pane.on_output(b"\r\nmore\r\nlast hit");
        // Between output and the next step the old grid matches move with
        // their text.
        assert_eq!(found_on(&pane, LineRef::History(1)), [(4, 7, true)]);
        while pane.find_step(1000).more {}
        let status = pane.find_status().unwrap();
        assert_eq!((status.index, status.count), (Some(2), 3));
        assert_eq!(found_on(&pane, LineRef::History(1)), [(4, 7, true)]);
        assert_eq!(found_on(&pane, LineRef::Grid(1)), [(5, 8, false)]);
    }

    #[test]
    fn clear_and_reattach_restart_the_search() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 2, &["hit", "hit"], 0));
        find_all(&mut pane, "hit", false);
        assert_eq!(pane.find_status().unwrap().count, 2);
        pane.on_output(b"\x1b[3J");
        while pane.find_step(1000).more {}
        assert_eq!(pane.find_status().unwrap().count, 0);
        pane.on_attached(&attach_ok(20, 2, &["hit", "hit", "hit"], 5));
        while pane.find_step(1000).more {}
        assert_eq!(pane.find_status().unwrap().count, 3);
    }

    #[test]
    fn find_pages_in_older_history_and_close_stops_it() {
        let mut pane = pane();
        let tail: Vec<String> = (0..500).map(|i| format!("l{i}")).collect();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        let mut ok = attach_ok(20, 2, &tail, 1000);
        ok.history_first_line = 0;
        pane.on_attached(&ok);
        // The view is far from the oldest line: no page yet.
        assert_eq!(pane.next_page_request(), None);
        pane.find_open("zzz", false);
        assert_eq!(pane.next_page_request(), None);
        while pane.find_step(1000).more {}
        // The search reached the oldest loaded line: it wants older ones.
        assert_eq!(pane.next_page_request(), Some((0, 1000)));
        let page = HistoryPage {
            agent_id: 1,
            first_line: 999,
            lines: vec![b"zzz".to_vec().into()],
        };
        pane.on_page(&page, 0);
        assert!(pane.find_step(1000).changed);
        assert_eq!(pane.find_status().unwrap().count, 1);
        assert_eq!(pane.next_page_request(), Some((0, 999)));
        pane.cancel_page_request();
        pane.scroll_to_bottom();
        pane.find_close();
        assert_eq!(pane.find_status(), None);
        assert_eq!(pane.next_page_request(), None);
    }

    #[test]
    fn page_lines_keep_one_line_of_context() {
        assert_eq!(page_lines(24), 23);
        assert_eq!(page_lines(2), 1);
        assert_eq!(page_lines(1), 1);
        assert_eq!(page_lines(0), 1);
    }

    #[test]
    fn keyboard_scrolls_by_page_and_to_the_ends() {
        let mut pane = pane();
        let tail: Vec<String> = (0..50).map(|i| format!("h{i}")).collect();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        pane.on_attached(&attach_ok(20, 10, &tail, 0));
        assert!(pane.can_scroll_view());
        assert!(pane.scroll_view(ViewScroll::PageUp));
        assert_eq!(pane.scroll_offset(), 9);
        pane.scroll_view(ViewScroll::PageUp);
        assert_eq!(pane.scroll_offset(), 18);
        pane.scroll_view(ViewScroll::PageDown);
        assert_eq!(pane.scroll_offset(), 9);
        pane.scroll_view(ViewScroll::PageDown);
        pane.scroll_view(ViewScroll::PageDown);
        assert_eq!(pane.scroll_offset(), 0);
        pane.scroll_view(ViewScroll::Top);
        assert_eq!(pane.scroll_offset(), 50);
        assert_eq!(pane.view_row(0), ViewRow::History(0));
        // Paging up stops at the oldest loaded line.
        pane.scroll_view(ViewScroll::PageUp);
        assert_eq!(pane.scroll_offset(), 50);
        // Scrolled up, output counts as unseen; the bottom clears it.
        pane.on_output(b"\r\n".repeat(12).as_slice());
        assert!(pane.new_output());
        pane.scroll_view(ViewScroll::Bottom);
        assert_eq!(pane.scroll_offset(), 0);
        assert_eq!(pane.unseen_lines(), 0);
    }

    #[test]
    fn keyboard_scrolling_leaves_the_alternate_screen_to_the_program() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 4, &["a", "b", "c"], 0));
        pane.on_output(b"\x1b[?1049h");
        assert!(!pane.can_scroll_view());
        for s in [
            ViewScroll::PageUp,
            ViewScroll::PageDown,
            ViewScroll::Top,
            ViewScroll::Bottom,
        ] {
            assert!(!pane.scroll_view(s), "{s:?}");
        }
        assert_eq!(pane.scroll_offset(), 0);
        pane.on_output(b"\x1b[?1049l");
        assert!(pane.scroll_view(ViewScroll::Top));
        assert_eq!(pane.scroll_offset(), 3);
    }

    #[test]
    fn select_all_takes_the_history_and_the_grid_down_to_its_last_text() {
        let mut pane = pane();
        assert!(!pane.select_all(), "nothing to select");
        assert!(!pane.has_selection());
        pane.on_attached(&attach_ok(20, 4, &["zero", "one  "], 7));
        pane.on_output(b"grid a\r\ngrid b");
        assert!(pane.select_all());
        assert_eq!(
            pane.selection_text().as_deref(),
            Some("zero\none\ngrid a\ngrid b")
        );
        assert!(pane.is_selected(LineRef::History(7), 0));
        assert!(!pane.is_selected(LineRef::Grid(2), 0));
        // No history: the grid alone.
        let mut pane = pane_with_grid_only();
        assert!(pane.select_all());
        assert_eq!(pane.selection_text().as_deref(), Some("only"));
        // A blank grid: the history alone.
        let mut pane = self::pane();
        pane.on_attached(&attach_ok(20, 3, &["old"], 0));
        assert!(pane.select_all());
        assert_eq!(pane.selection_text().as_deref(), Some("old"));
    }

    fn pane_with_grid_only() -> PaneModel {
        let mut pane = pane();
        pane.on_attached(&attach_ok(20, 3, &[], 0));
        pane.on_output(b"only");
        pane
    }

    #[test]
    fn clear_scrollback_empties_the_view_only() {
        let mut pane = pane();
        let mut ok = attach_ok(20, 3, &["e1", "x", "e2"], 10);
        ok.history_first_line = 2;
        ok.history_generation = 4;
        pane.on_attached(&ok);
        pane.on_output(b"e3");
        pane.scroll_lines(2);
        pane.select_start(SelPoint {
            line: LineRef::History(11),
            col: 0,
        });
        pane.select_extend(SelPoint {
            line: LineRef::History(12),
            col: 1,
        });
        find_all(&mut pane, "e", false);
        assert_eq!(pane.find_status().unwrap().count, 3);
        pane.clear_scrollback();
        assert!(pane.scrollback().is_empty());
        assert_eq!(pane.scrollback().generation(), 4, "the node's numbering");
        assert_eq!(pane.scroll_offset(), 0);
        assert!(!pane.has_selection());
        assert!(!pane.can_scroll_view());
        // The grid is the agent's: it stays.
        assert_eq!(pane.view_row(0), ViewRow::Grid(0));
        find_all(&mut pane, "e", false);
        assert_eq!(pane.find_status().unwrap().count, 1);
        // Nothing older is paged back in.
        assert_eq!(pane.next_page_request(), None);
        // Output after the clear is history as usual.
        pane.on_output(b"\r\n\r\n\r\nnew");
        assert_eq!(pane.scrollback().len(), 1);
        // A re-attach rebuilds the history from the node.
        pane.on_attached(&ok);
        assert_eq!(pane.scrollback().len(), 3);
    }

    #[test]
    fn clear_scrollback_keeps_a_selection_on_the_grid() {
        let mut pane = pane_with_grid_only();
        pane.select_word(SelPoint {
            line: LineRef::Grid(0),
            col: 1,
        });
        pane.clear_scrollback();
        assert_eq!(pane.selection_text().as_deref(), Some("only"));
    }

    fn at(line: LineRef, col: u16) -> SelPoint {
        SelPoint { line, col }
    }

    /// 10 history lines `h0..h9` over a 20×3 grid.
    fn pane_for_drag() -> PaneModel {
        let mut pane = pane();
        let tail: Vec<String> = (0..10).map(|i| format!("h{i}")).collect();
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        pane.on_attached(&attach_ok(20, 3, &tail, 0));
        pane.on_output(b"g0\r\ng1\r\ng2");
        pane
    }

    #[test]
    fn drag_scroll_up_moves_the_view_and_extends_to_its_top_row() {
        let mut pane = pane_for_drag();
        pane.select_start(at(LineRef::Grid(1), 2));
        assert!(pane.drag_scroll(2, 1));
        assert_eq!(pane.scroll_offset(), 2);
        // Row 0 now shows h8: the selection runs from there to the anchor.
        assert_eq!(pane.selection_text().as_deref(), Some("8\nh9\ng0\ng1"));
        // Past the oldest line it clamps, then stops moving.
        assert!(pane.drag_scroll(100, 0));
        assert_eq!(pane.scroll_offset(), 10);
        assert_eq!(pane.selection_text().unwrap().lines().next(), Some("h0"));
        assert!(!pane.drag_scroll(1, 0));
    }

    #[test]
    fn drag_scroll_down_extends_to_the_bottom_row_and_stops_at_the_grid() {
        let mut pane = pane_for_drag();
        pane.scroll_to(5);
        pane.select_start(at(LineRef::History(5), 0));
        assert!(pane.drag_scroll(-2, 2));
        assert_eq!(pane.scroll_offset(), 3);
        // Row 2 now shows h9.
        assert_eq!(pane.selection_text().as_deref(), Some("h5\nh6\nh7\nh8\nh9"));
        assert!(pane.drag_scroll(-10, 1));
        assert_eq!(pane.scroll_offset(), 0);
        assert_eq!(pane.selection_text().unwrap().lines().last(), Some("g2"));
        assert!(
            !pane.drag_scroll(-1, 1),
            "following the grid: nothing below"
        );
    }

    #[test]
    fn drag_scroll_needs_a_selection_and_history_off_the_alternate_screen() {
        let mut pane = pane_for_drag();
        assert!(!pane.drag_scroll(1, 0), "no selection");
        assert_eq!(pane.scroll_offset(), 0);
        pane.select_start(at(LineRef::Grid(0), 0));
        pane.on_output(b"\x1b[?1049h");
        pane.select_start(at(LineRef::Grid(0), 0));
        assert!(
            !pane.drag_scroll(1, 0),
            "the alternate screen has no history"
        );
        assert_eq!(pane.scroll_offset(), 0);
    }

    #[test]
    fn shift_click_extends_a_selection_from_its_anchor() {
        let mut pane = pane_for_drag();
        pane.select_start(at(LineRef::Grid(0), 1));
        pane.select_extend(at(LineRef::Grid(1), 1));
        assert_eq!(pane.selection_text().as_deref(), Some("0\ng1"));
        // A later Shift-click moves the moving end, the anchor stays.
        pane.select_extend(at(LineRef::Grid(2), 0));
        assert_eq!(pane.selection_text().as_deref(), Some("0\ng1\ng"));
        pane.select_extend(at(LineRef::History(9), 0));
        assert_eq!(pane.selection_text().as_deref(), Some("h9\ng0"));
    }
}
