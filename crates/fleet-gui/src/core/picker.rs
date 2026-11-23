//! A fuzzy picker's state: items, the query, what matches it in which
//! order, and the selected row. The command palette uses it; so can any
//! chooser (split chooser, agent switcher). The shell draws it as a
//! floating list over a query field.

use std::collections::VecDeque;

use super::fuzzy::fuzzy_match;

/// How many picks [`Recents`] remembers.
pub const RECENTS_CAP: usize = 32;
/// The score bonus of the most recent pick; each older one gets one less.
pub const RECENT_BONUS: i32 = 12;

/// A row of a picker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickerItem {
    /// Identifies the item across rebuilds (what [`Recents`] remembers).
    pub key: String,
    pub title: String,
    /// Secondary text after the title (a path, a status); matched too.
    pub detail: String,
    /// Matched, not shown (labels, aliases).
    pub keywords: String,
    /// Shown at the row's end (a shortcut like `⇧⌘D`); not matched.
    pub shortcut: Option<String>,
    /// A dim line under the row (an agent's last output); not matched.
    pub preview: String,
}

/// A row that matches the query, with where it matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked {
    /// Index into [`Picker::items`].
    pub item: usize,
    pub score: i32,
    /// Byte offsets of the matched characters in the title.
    pub title_hits: Vec<usize>,
    /// Byte offsets of the matched characters in the detail.
    pub detail_hits: Vec<usize>,
}

/// The keys of recent picks, most recent first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recents(VecDeque<String>);

impl Recents {
    /// Remembers a pick of `key`, as the most recent.
    pub fn touch(&mut self, key: &str) {
        self.0.retain(|k| k != key);
        self.0.push_front(key.to_owned());
        self.0.truncate(RECENTS_CAP);
    }

    /// How recent `key` is: 0 for the latest pick; `None` if not recent.
    pub fn rank(&self, key: &str) -> Option<usize> {
        self.0.iter().position(|k| k == key)
    }

    fn bonus(&self, key: &str) -> i32 {
        self.rank(key)
            .map_or(0, |r| (RECENT_BONUS - r as i32).max(0))
    }
}

/// A picker over `items`: [`Picker::ranked`] is what the query matches,
/// best first, and one of them is selected.
#[derive(Clone, Debug, Default)]
pub struct Picker {
    items: Vec<PickerItem>,
    recents: Recents,
    query: String,
    ranked: Vec<Ranked>,
    selected: usize,
    /// The rows are given ([`Picker::set_listed`]), not ranked by the query.
    listed: bool,
}

impl Picker {
    pub fn new(items: Vec<PickerItem>, recents: Recents) -> Self {
        let mut p = Picker {
            items,
            recents,
            ..Picker::default()
        };
        p.rank();
        p
    }

    pub fn items(&self) -> &[PickerItem] {
        &self.items
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The matching rows, best first. With an empty query: recent picks
    /// first, most recent first, then the rest in the items' order. With
    /// a query: by score (a recent pick gets a small bonus), then
    /// recency, then the items' order.
    pub fn ranked(&self) -> &[Ranked] {
        &self.ranked
    }

    /// The selected row (an index into [`Picker::ranked`]).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The selected item, if anything matches.
    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.ranked.get(self.selected).map(|r| &self.items[r.item])
    }

    /// Sets the query; the best match is selected. A listed picker keeps
    /// its rows (its owner lists new ones).
    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_owned();
        if !self.listed {
            self.rank();
        }
    }

    /// Shows `items` as they are, in order, each title highlighted at the
    /// byte offsets `title_hits[i]`; the query no longer ranks them. The
    /// first row is selected.
    pub fn set_listed(&mut self, items: Vec<PickerItem>, title_hits: Vec<Vec<usize>>) {
        self.listed = true;
        self.ranked = (0..items.len())
            .map(|i| Ranked {
                item: i,
                score: 0,
                title_hits: title_hits.get(i).cloned().unwrap_or_default(),
                detail_hits: Vec::new(),
            })
            .collect();
        self.items = items;
        self.selected = 0;
    }

    /// Moves the selection by `delta` rows, wrapping around.
    pub fn move_selection(&mut self, delta: isize) {
        let n = self.ranked.len() as isize;
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
    }

    /// Selects row `row` of [`Picker::ranked`] (a hover or click).
    pub fn select(&mut self, row: usize) {
        if row < self.ranked.len() {
            self.selected = row;
        }
    }

    fn rank(&mut self) {
        let mut ranked: Vec<(Ranked, usize)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let title_end = item.title.len();
            let detail_start = title_end + 1;
            let detail_end = detail_start + item.detail.len();
            let text = format!("{} {} {}", item.title, item.detail, item.keywords);
            let Some(m) = fuzzy_match(&self.query, &text) else {
                continue;
            };
            let title_hits = m.positions.iter().copied().filter(|&p| p < title_end);
            let detail_hits = m
                .positions
                .iter()
                .filter(|&&p| (detail_start..detail_end).contains(&p))
                .map(|p| p - detail_start);
            let recency = self.recents.rank(&item.key).unwrap_or(usize::MAX);
            ranked.push((
                Ranked {
                    item: i,
                    score: m.score + self.recents.bonus(&item.key),
                    title_hits: title_hits.collect(),
                    detail_hits: detail_hits.collect(),
                },
                recency,
            ));
        }
        ranked.sort_by_key(|(r, recency)| (std::cmp::Reverse(r.score), *recency, r.item));
        self.ranked = ranked.into_iter().map(|(r, _)| r).collect();
        self.selected = 0;
    }
}

/// `hits` (byte offsets of matched characters in `text`) as byte ranges
/// to highlight, adjacent characters merged into one range.
pub fn hit_ranges(text: &str, hits: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for &start in hits {
        let Some(c) = text.get(start..).and_then(|t| t.chars().next()) else {
            continue;
        };
        let end = start + c.len_utf8();
        match out.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => out.push(start..end),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, title: &str, detail: &str) -> PickerItem {
        PickerItem {
            key: key.into(),
            title: title.into(),
            detail: detail.into(),
            ..PickerItem::default()
        }
    }

    fn items() -> Vec<PickerItem> {
        vec![
            item("split-right", "Split Right", ""),
            item("split-down", "Split Down", ""),
            item("new-agent", "New Agent…", ""),
            item("agent:7", "claude in ~/src/fleetd", "running"),
        ]
    }

    fn keys(p: &Picker) -> Vec<&str> {
        p.ranked()
            .iter()
            .map(|r| p.items()[r.item].key.as_str())
            .collect()
    }

    #[test]
    fn a_listed_picker_keeps_its_rows_whatever_the_query() {
        let mut p = Picker::new(Vec::new(), Recents::default());
        p.set_listed(
            vec![item("b", "zzz", ""), item("a", "error", "")],
            vec![vec![], vec![0, 1]],
        );
        p.set_query("error");
        let keys: Vec<&str> = p
            .ranked()
            .iter()
            .map(|r| p.items()[r.item].key.as_str())
            .collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(p.ranked()[1].title_hits, [0, 1]);
        assert_eq!(p.selected_item().unwrap().key, "b");
        p.move_selection(1);
        assert_eq!(p.selected_item().unwrap().key, "a");
        p.set_listed(Vec::new(), Vec::new());
        assert_eq!(p.selected_item(), None);
    }

    #[test]
    fn an_empty_query_lists_everything_in_order() {
        let p = Picker::new(items(), Recents::default());
        assert_eq!(
            keys(&p),
            ["split-right", "split-down", "new-agent", "agent:7"]
        );
        assert_eq!(p.selected(), 0);
        assert_eq!(p.selected_item().unwrap().key, "split-right");
    }

    #[test]
    fn the_query_filters_and_ranks_best_first() {
        let mut p = Picker::new(items(), Recents::default());
        p.set_query("sd");
        assert_eq!(keys(&p)[0], "split-down");
        p.set_query("zzz");
        assert!(p.ranked().is_empty());
        assert_eq!(p.selected_item(), None);
    }

    #[test]
    fn recent_picks_come_first_and_win_ties() {
        let mut recents = Recents::default();
        recents.touch("split-right");
        recents.touch("agent:7");
        let mut p = Picker::new(items(), recents);
        assert_eq!(
            keys(&p),
            ["agent:7", "split-right", "split-down", "new-agent"]
        );
        // "split" matches both splits equally: the recent one first.
        p.set_query("split");
        assert_eq!(keys(&p)[..2], ["split-right", "split-down"]);
        // A much better match still beats a recent pick.
        p.set_query("split down");
        assert_eq!(keys(&p)[0], "split-down");
    }

    #[test]
    fn recents_move_to_the_front_and_are_capped() {
        let mut r = Recents::default();
        r.touch("a");
        r.touch("b");
        r.touch("a");
        assert_eq!(r.rank("a"), Some(0));
        assert_eq!(r.rank("b"), Some(1));
        assert_eq!(r.rank("c"), None);
        for i in 0..RECENTS_CAP {
            r.touch(&i.to_string());
        }
        assert_eq!(r.rank("a"), None);
        assert_eq!(r.rank("0"), Some(RECENTS_CAP - 1));
    }

    #[test]
    fn the_selection_wraps_and_resets_on_a_new_query() {
        let mut p = Picker::new(items(), Recents::default());
        p.move_selection(-1);
        assert_eq!(p.selected(), 3);
        p.move_selection(1);
        assert_eq!(p.selected(), 0);
        p.move_selection(2);
        p.select(1);
        assert_eq!(p.selected(), 1);
        p.select(99);
        assert_eq!(p.selected(), 1);
        p.set_query("s");
        assert_eq!(p.selected(), 0);
        // Nothing to select: nothing happens.
        p.set_query("zzz");
        p.move_selection(1);
        assert_eq!(p.selected(), 0);
    }

    #[test]
    fn hits_are_split_into_title_and_detail() {
        let mut p = Picker::new(items(), Recents::default());
        p.set_query("claude run");
        let r = &p.ranked()[0];
        assert_eq!(p.items()[r.item].key, "agent:7");
        assert_eq!(r.title_hits, [0, 1, 2, 3, 4, 5]);
        assert_eq!(r.detail_hits, [0, 1, 2]);
    }

    #[test]
    fn hits_become_merged_ranges() {
        assert_eq!(hit_ranges("Split Right", &[0, 1, 6]), [0..2, 6..7]);
        // Multi-byte characters span their bytes.
        assert_eq!(hit_ranges("café x", &[3, 6]), [3..5, 6..7]);
        assert!(hit_ranges("ab", &[]).is_empty());
        // An offset off the text is skipped.
        assert_eq!(hit_ranges("ab", &[1, 9]), vec![1..2]);
    }

    #[test]
    fn keywords_match_but_are_not_hits() {
        let mut p = Picker::new(
            vec![PickerItem {
                keywords: "project=fleetd".into(),
                ..item("agent:1", "sh", "")
            }],
            Recents::default(),
        );
        p.set_query("project");
        assert_eq!(p.ranked().len(), 1);
        assert!(p.ranked()[0].title_hits.is_empty());
        assert!(p.ranked()[0].detail_hits.is_empty());
    }
}
