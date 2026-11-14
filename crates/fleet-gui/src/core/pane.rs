//! One terminal pane: the client grid, its history and the view over them.
//!
//! No I/O: the session layer feeds it attach results, output, role changes
//! and history pages, and takes replies and page requests from it. The grid
//! is a client [`Terminal`]; lines that scroll off it go to [`Scrollback`]
//! numbered as the node numbers them (decisions 0021 and 0022).

use fleet_proto::{AttachOk, HistoryPage};
use fleet_term::{Cell, HistoryEvent, Palette, Terminal, parse_history_line};

use super::scrollback::Scrollback;
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

pub struct PaneModel {
    palette: Palette,
    term: Terminal,
    scrollback: Scrollback,
    state: PaneState,
    primary: bool,
    page_in_flight: bool,
    /// History lines the view is scrolled up by; 0 follows the grid.
    scroll_offset: u64,
    selection: Option<Selection>,
}

impl PaneModel {
    pub fn new(palette: Palette) -> Self {
        Self {
            palette,
            term: Terminal::new_client(INITIAL_SIZE.0, INITIAL_SIZE.1, palette),
            scrollback: Scrollback::default(),
            state: PaneState::Attaching,
            primary: false,
            page_in_flight: false,
            scroll_offset: 0,
            selection: None,
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
        self.scrollback = Scrollback::new(
            ok.history_generation,
            ok.tail_first_line,
            ok.history_first_line,
        );
        for line in &ok.history_tail {
            self.scrollback.push_back(line.to_vec());
        }
        self.term.feed(&ok.snapshot);
        self.term.drain_history();
        self.term.take_replies();
        self.primary = ok.primary;
        self.state = PaneState::Live;
        self.page_in_flight = false;
        self.scroll_offset = 0;
        self.selection = None;
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
    /// line, older history exists and no request is in flight.
    pub fn next_page_request(&mut self) -> Option<(u64, u64)> {
        let sb = &self.scrollback;
        if self.page_in_flight || sb.fully_loaded() {
            return None;
        }
        let top = sb.end_line() - self.scroll_offset;
        if top - sb.first_line() > PREFETCH_LINES {
            return None;
        }
        self.page_in_flight = true;
        Some((sb.generation(), sb.first_line()))
    }

    /// A page answering the request made with `generation`. Dropped if a
    /// clear happened since or it does not join the loaded lines; a page of
    /// the current generation that adds nothing older ends the paging.
    pub fn on_page(&mut self, page: &HistoryPage, generation: u64) {
        self.page_in_flight = false;
        let before = self.scrollback.first_line();
        let lines = page.lines.iter().map(|l| l.to_vec()).collect();
        self.scrollback
            .prepend_page(generation, page.first_line, lines);
        if generation == self.scrollback.generation() && self.scrollback.first_line() == before {
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
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// History lines the view is scrolled up by; 0 follows the live grid.
    pub fn scroll_offset(&self) -> u64 {
        self.scroll_offset
    }

    /// Replaces `out` with what each of `rows` visible rows shows, top to
    /// bottom.
    pub fn view_rows(&self, rows: u16, out: &mut Vec<ViewRow>) {
        out.clear();
        let top = self.scrollback.end_line() - self.scroll_offset;
        out.extend((0..u64::from(rows)).map(|i| {
            let abs = top + i;
            match abs.checked_sub(self.scrollback.end_line()) {
                Some(row) => ViewRow::Grid(row as u16),
                None => ViewRow::History(abs),
            }
        }));
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

    /// After the grid changed: history events into the scrollback, the view
    /// pinned, a selection on the grid dropped, observer replies discarded.
    fn grid_changed(&mut self) {
        for event in self.term.drain_history() {
            match event {
                HistoryEvent::Line(line) => {
                    self.scrollback.push_back(line);
                    if self.scroll_offset > 0 {
                        self.scroll_offset += 1;
                    }
                }
                HistoryEvent::Cleared => {
                    self.scrollback.clear();
                    self.scroll_offset = 0;
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
        pane.on_attached(&attach_ok(10, 3, &["t0", "t1", "t2"], 100));
        assert_eq!(pane.state(), PaneState::Live);
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

    #[test]
    fn resize_moves_rows_pushed_off_into_history() {
        let mut pane = pane();
        pane.on_attached(&attach_ok(10, 3, &[], 40));
        pane.on_output(b"1\r\n2\r\n3");
        pane.on_resized(10, 2);
        assert_eq!(history_texts(&pane), ["1"]);
        assert_eq!(pane.scrollback().first_line(), 40);
    }
}
