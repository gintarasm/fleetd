//! A one-line text field's editing model: the text, the selection (an
//! anchor and a caret), the IME's marked text and undo/redo, with the
//! macOS editing commands (⌥ moves by word, ⌘ to the ends, ⌘⌫ deletes to
//! the start). Offsets are byte offsets on char boundaries; the IME's
//! UTF-16 offsets are converted at the edge.

use std::ops::Range;

/// How the caret moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Movement {
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
}

/// Consecutive edits of one kind undo together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    anchor: usize,
    caret: usize,
}

/// At most this many undo steps are kept.
const MAX_UNDO: usize = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextEdit {
    text: String,
    /// The fixed end of the selection.
    anchor: usize,
    /// The moving end: where the caret is drawn.
    caret: usize,
    /// The IME's uncommitted text.
    marked: Option<Range<usize>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The kind of the last edit, while the next one may join it.
    last: Option<EditKind>,
}

/// `text` without line breaks.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], "")
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl TextEdit {
    /// `text` with the caret at its end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        TextEdit {
            text,
            anchor: end,
            caret: end,
            ..Default::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The selection, start to end (empty: the caret).
    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    /// The caret: the selection's moving end.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selection was made leftwards (the caret is at its start).
    pub fn is_reversed(&self) -> bool {
        self.caret < self.anchor
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Replaces all the text (a pick from a list); undoable.
    pub fn set_text(&mut self, text: &str) {
        self.checkpoint(EditKind::Other);
        self.text = text.to_owned();
        self.anchor = self.text.len();
        self.caret = self.anchor;
        self.marked = None;
    }

    pub fn select_all(&mut self) {
        self.marked = None;
        self.anchor = 0;
        self.caret = self.text.len();
        self.last = None;
    }

    /// Puts the caret at `at` (clamped onto a char boundary); `extend`
    /// keeps the anchor (Shift, or a drag).
    pub fn set_caret(&mut self, at: usize, extend: bool) {
        let at = self.floor(at);
        self.marked = None;
        self.caret = at;
        if !extend {
            self.anchor = at;
        }
        self.last = None;
    }

    /// Selects the word at `at` (a double click), or the one character
    /// there if it is not part of a word. At the end: the one before.
    pub fn select_word_at(&mut self, at: usize) {
        let at = self.floor(at);
        let hit = match self.text[at..].chars().next() {
            Some(c) => Some((at, c)),
            None => self.text[..at].char_indices().next_back(),
        };
        let Some((pos, c)) = hit else {
            return;
        };
        self.marked = None;
        let (start, end) = if is_word(c) {
            let start = self.text[..pos]
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_word(*c))
                .last()
                .map_or(pos, |(i, _)| i);
            let end = self.text[pos..]
                .char_indices()
                .find(|(_, c)| !is_word(*c))
                .map_or(self.text.len(), |(i, _)| pos + i);
            (start, end)
        } else {
            (pos, pos + c.len_utf8())
        };
        self.anchor = start;
        self.caret = end;
        self.last = None;
    }

    /// Moves the caret; `extend` (Shift) keeps the anchor. Without it, a
    /// selection collapses to its start or end for Left or Right.
    pub fn move_caret(&mut self, m: Movement, extend: bool) {
        let sel = self.selection();
        let to = match m {
            Movement::Left if !extend && !sel.is_empty() => sel.start,
            Movement::Right if !extend && !sel.is_empty() => sel.end,
            Movement::Left => self.prev_char(self.caret),
            Movement::Right => self.next_char(self.caret),
            Movement::WordLeft => self.prev_word(self.caret),
            Movement::WordRight => self.next_word(self.caret),
            Movement::Home => 0,
            Movement::End => self.text.len(),
        };
        self.set_caret(to, extend);
    }

    /// Types `text` over the selection (line breaks dropped: one line).
    pub fn insert(&mut self, text: &str) {
        let text = &one_line(text);
        let kind = if text.chars().count() == 1 && !text.contains(char::is_whitespace) {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        self.checkpoint(kind);
        let sel = self.selection();
        self.splice(sel, text);
    }

    /// ⌫: the selection, else the character before the caret.
    pub fn backspace(&mut self) {
        let sel = self.selection();
        let range = if sel.is_empty() {
            self.prev_char(self.caret)..self.caret
        } else {
            sel
        };
        self.delete(range);
    }

    /// ⌦: the selection, else the character after the caret.
    pub fn delete_forward(&mut self) {
        let sel = self.selection();
        let range = if sel.is_empty() {
            self.caret..self.next_char(self.caret)
        } else {
            sel
        };
        self.delete(range);
    }

    /// ⌥⌫: the selection, else back to the start of the word.
    pub fn delete_word_back(&mut self) {
        let sel = self.selection();
        let range = if sel.is_empty() {
            self.prev_word(self.caret)..self.caret
        } else {
            sel
        };
        self.delete(range);
    }

    /// ⌘⌫: the selection, else everything before the caret.
    pub fn delete_to_start(&mut self) {
        let sel = self.selection();
        let range = if sel.is_empty() { 0..self.caret } else { sel };
        self.delete(range);
    }

    /// ⌃K: the selection, else everything after the caret.
    pub fn delete_to_end(&mut self) {
        let sel = self.selection();
        let range = if sel.is_empty() {
            self.caret..self.text.len()
        } else {
            sel
        };
        self.delete(range);
    }

    /// ⌘X: removes and returns the selection, if any.
    pub fn cut(&mut self) -> Option<String> {
        let sel = self.selection();
        if sel.is_empty() {
            return None;
        }
        let text = self.text[sel.clone()].to_owned();
        self.checkpoint(EditKind::Other);
        self.splice(sel, "");
        Some(text)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.restore(prev);
        self.redo.push(now);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.restore(next);
        self.undo.push(now);
        true
    }

    // The IME, in UTF-16 offsets.

    /// UTF-16 offset of byte offset `at`.
    pub fn to_utf16(&self, at: usize) -> usize {
        self.text[..self.floor(at)].encode_utf16().count()
    }

    /// Byte offset of UTF-16 offset `at`, clamped to the text.
    pub fn from_utf16(&self, at: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.text.char_indices() {
            if seen >= at {
                return i;
            }
            seen += c.len_utf16();
        }
        self.text.len()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        let start = self.from_utf16(r.start);
        start..self.from_utf16(r.end).max(start)
    }

    fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.to_utf16(r.start)..self.to_utf16(r.end)
    }

    pub fn selection_utf16(&self) -> Range<usize> {
        self.range_to_utf16(&self.selection())
    }

    pub fn marked_utf16(&self) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    /// The text in UTF-16 range `r` (clamped), and that range as clamped.
    pub fn text_for_utf16(&self, r: &Range<usize>) -> (String, Range<usize>) {
        let bytes = self.range_from_utf16(r);
        let text = self.text[bytes.clone()].to_owned();
        (text, self.range_to_utf16(&bytes))
    }

    /// Commits `text` over UTF-16 `range`, or else the marked text, or
    /// else the selection (typing, and the IME's final text).
    pub fn replace_utf16(&mut self, range: Option<Range<usize>>, text: &str) {
        let composing = self.marked.is_some();
        let target = range
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked.take())
            .unwrap_or_else(|| self.selection());
        self.marked = None;
        if composing {
            // The composition's start was the undo step.
            self.splice(target, &one_line(text));
            self.redo.clear();
            self.last = Some(EditKind::Other);
            return;
        }
        self.anchor = target.start;
        self.caret = target.end;
        self.insert(text);
    }

    /// The IME's uncommitted text: replaces UTF-16 `range` (or the marked
    /// text, or the selection) and marks it; `selected` is the selection
    /// inside it, UTF-16 relative to its start.
    pub fn replace_and_mark_utf16(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
    ) {
        let target = range
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        if self.marked.is_none() {
            self.checkpoint(EditKind::Other);
        }
        let text = &one_line(text);
        self.splice(target.clone(), text);
        let start = target.start;
        self.marked = (!text.is_empty()).then(|| start..start + text.len());
        if let Some(sel) = selected {
            let base = self.to_utf16(start);
            let from = self.from_utf16(base + sel.start);
            let to = self.from_utf16(base + sel.end);
            self.anchor = from.min(start + text.len());
            self.caret = to.min(start + text.len());
        }
    }

    /// The IME gave up composing: the marked text stays as typed.
    pub fn unmark(&mut self) {
        self.marked = None;
    }

    // Internals.

    fn floor(&self, at: usize) -> usize {
        let mut at = at.min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn prev_char(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_char(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| at + c.len_utf8())
    }

    /// The start of the word before `at` (skipping non-word characters).
    fn prev_word(&self, at: usize) -> usize {
        let mut seen_word = false;
        for (i, c) in self.text[..at].char_indices().rev() {
            if is_word(c) {
                seen_word = true;
            } else if seen_word {
                return i + c.len_utf8();
            }
        }
        0
    }

    /// The end of the word after `at` (skipping non-word characters).
    fn next_word(&self, at: usize) -> usize {
        let mut seen_word = false;
        for (i, c) in self.text[at..].char_indices() {
            if is_word(c) {
                seen_word = true;
            } else if seen_word {
                return at + i;
            }
        }
        self.text.len()
    }

    fn delete(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.checkpoint(EditKind::Deleting);
        self.splice(range, "");
    }

    fn splice(&mut self, range: Range<usize>, text: &str) {
        self.text.replace_range(range.clone(), text);
        let at = range.start + text.len();
        self.anchor = at;
        self.caret = at;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            anchor: self.anchor,
            caret: self.caret,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.text = s.text;
        self.anchor = s.anchor;
        self.caret = s.caret;
        self.marked = None;
        self.last = None;
    }

    /// Records the state before an edit of `kind`, unless it continues
    /// the last edit of the same kind (typing a word undoes as one).
    /// Every edit but the IME's own ends a composition.
    fn checkpoint(&mut self, kind: EditKind) {
        self.marked = None;
        let joins = kind != EditKind::Other && self.last == Some(kind);
        if !joins {
            self.undo.push(self.snapshot());
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last = Some(kind);
    }
}

#[cfg(test)]
mod tests {
    use super::{Movement as M, TextEdit};

    fn typed(s: &str) -> TextEdit {
        let mut e = TextEdit::new("");
        for c in s.chars() {
            e.insert(&c.to_string());
        }
        e
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut e = TextEdit::new("held");
        e.move_caret(M::Left, false);
        e.insert("l");
        assert_eq!(e.text(), "helld");
        e.move_caret(M::Home, false);
        e.insert(">");
        assert_eq!(e.text(), ">helld");
        assert_eq!(e.caret(), 1);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut e = TextEdit::new("hello world");
        e.select_all();
        assert_eq!(e.selected_text(), "hello world");
        e.insert("x");
        assert_eq!(e.text(), "x");
        assert!(e.selection().is_empty());
    }

    #[test]
    fn shift_moves_extend_and_plain_moves_collapse() {
        let mut e = TextEdit::new("abcdef");
        e.move_caret(M::Left, true);
        e.move_caret(M::Left, true);
        assert_eq!(e.selected_text(), "ef");
        assert!(e.is_reversed());
        e.move_caret(M::Left, false);
        assert_eq!(e.selection(), 4..4, "Left collapses to the start");
        e.move_caret(M::Home, true);
        assert_eq!(e.selected_text(), "abcd");
        e.move_caret(M::Right, false);
        assert_eq!(e.caret(), 4, "Right collapses to the end");
        e.move_caret(M::End, false);
        assert_eq!(e.caret(), 6);
    }

    #[test]
    fn word_moves_skip_punctuation_like_macos() {
        let mut e = TextEdit::new("~/src/fleet-gui now");
        e.move_caret(M::WordLeft, false);
        assert_eq!(e.caret(), 16, "start of `now`");
        e.move_caret(M::WordLeft, false);
        assert_eq!(e.caret(), 12, "start of `gui`");
        e.move_caret(M::WordLeft, false);
        assert_eq!(e.caret(), 6, "start of `fleet`");
        e.move_caret(M::WordRight, false);
        assert_eq!(e.caret(), 11, "end of `fleet`");
        e.move_caret(M::Home, false);
        e.move_caret(M::WordRight, false);
        assert_eq!(e.caret(), 5, "end of `src`");
    }

    #[test]
    fn deletions() {
        let mut e = TextEdit::new("one two three");
        e.backspace();
        assert_eq!(e.text(), "one two thre");
        e.delete_word_back();
        assert_eq!(e.text(), "one two ");
        e.move_caret(M::Home, false);
        e.delete_forward();
        assert_eq!(e.text(), "ne two ");
        e.move_caret(M::End, false);
        e.move_caret(M::WordLeft, false);
        e.delete_to_start();
        assert_eq!(e.text(), "two ");
        assert_eq!(e.caret(), 0);
        // Nothing before the caret: nothing happens, nothing to undo.
        let before = e.clone();
        e.backspace();
        assert_eq!(e, before);
        e.move_caret(M::Right, false);
        e.delete_to_end();
        assert_eq!(e.text(), "t");
    }

    #[test]
    fn edits_while_composing_drop_the_stale_marked_text() {
        let mut e = TextEdit::new("abc");
        e.replace_and_mark_utf16(None, "にほん", None);
        e.select_all();
        assert_eq!(e.cut().as_deref(), Some("abcにほん"));
        assert_eq!(e.marked(), None);
        e.replace_and_mark_utf16(None, "に", None);
        assert_eq!(e.text(), "に");
        e.replace_utf16(None, "日");
        assert_eq!(e.text(), "日");
        // Every other edit too.
        e.replace_and_mark_utf16(None, "x", None);
        e.set_text("");
        assert_eq!(e.marked(), None);
        e.replace_and_mark_utf16(None, "y", None);
        e.backspace();
        assert_eq!(e.marked(), None);
        e.replace_utf16(None, "z");
        assert_eq!(e.text(), "z");
    }

    #[test]
    fn the_field_stays_one_line() {
        let mut e = TextEdit::new("");
        e.insert("a\nb\r\n");
        assert_eq!(e.text(), "ab");
        e.replace_and_mark_utf16(None, "c\n", None);
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn multibyte_text_moves_by_character() {
        let mut e = TextEdit::new("aé🙂b");
        e.move_caret(M::Left, false);
        e.move_caret(M::Left, false);
        e.backspace();
        assert_eq!(e.text(), "a🙂b");
        assert_eq!(e.to_utf16(e.caret()), 1);
        e.set_caret(3, false);
        assert_eq!(e.caret(), 1, "clamped onto a char boundary");
    }

    #[test]
    fn typing_undoes_by_word_and_redo_comes_back() {
        let mut e = typed("ab");
        e.insert(" ");
        let mut more = "cd".chars();
        e.insert(&more.next().unwrap().to_string());
        e.insert(&more.next().unwrap().to_string());
        assert_eq!(e.text(), "ab cd");
        assert!(e.undo());
        assert_eq!(e.text(), "ab ");
        assert!(e.undo());
        assert_eq!(e.text(), "ab");
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(!e.undo());
        assert!(e.redo());
        assert_eq!(e.text(), "ab");
        // A new edit drops the redo history.
        e.insert("!");
        assert!(!e.redo());
    }

    #[test]
    fn cut_and_set_text_are_undoable() {
        let mut e = TextEdit::new("hello");
        e.move_caret(M::WordLeft, true);
        assert_eq!(e.cut().as_deref(), Some("hello"));
        assert_eq!(e.text(), "");
        assert_eq!(e.cut(), None);
        e.set_text("/tmp");
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(e.undo());
        assert_eq!(e.text(), "hello");
        assert_eq!(e.selected_text(), "hello", "the selection comes back");
    }

    #[test]
    fn double_click_selects_a_word_or_a_run() {
        let mut e = TextEdit::new("cd ~/my_dir now");
        e.select_word_at(6);
        assert_eq!(e.selected_text(), "my_dir");
        e.select_word_at(4);
        assert_eq!(e.selected_text(), "/", "not a word: just the character");
        e.select_word_at(15);
        assert_eq!(e.selected_text(), "now", "at the end: the word before");
    }

    #[test]
    fn ime_composition_replaces_and_commits() {
        let mut e = TextEdit::new("x");
        e.replace_and_mark_utf16(None, "に", Some(1..1));
        assert_eq!(e.text(), "xに");
        assert_eq!(e.marked_utf16(), Some(1..2));
        e.replace_and_mark_utf16(None, "日本", Some(2..2));
        assert_eq!(e.text(), "x日本");
        assert_eq!(e.marked_utf16(), Some(1..3));
        assert_eq!(e.selection_utf16(), 3..3);
        e.replace_utf16(None, "日本語");
        assert_eq!(e.text(), "x日本語");
        assert_eq!(e.marked(), None);
        assert_eq!(e.caret(), e.text().len());
        // The whole composition undoes as one.
        assert!(e.undo());
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn ime_ranges_and_text_for_range_clamp() {
        let mut e = TextEdit::new("a🙂c");
        assert_eq!(e.text_for_utf16(&(1..3)), ("🙂".to_owned(), 1..3));
        assert_eq!(e.text_for_utf16(&(2..99)).1, 3..4, "inside a pair: clamped");
        e.replace_utf16(Some(0..1), "A");
        assert_eq!(e.text(), "A🙂c");
        e.replace_and_mark_utf16(None, "", None);
        assert_eq!(e.marked(), None);
        e.unmark();
        assert_eq!(e.text(), "A🙂c");
    }
}
