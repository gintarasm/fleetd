//! Workspace tabs: each tab is its own split layout with its own focused
//! pane; one tab is shown at a time (ADR 0061). Pane ids are unique
//! across tabs, so a pane names one terminal whichever tab holds it.

use serde::{Deserialize, Serialize};

use super::layout::{Axis, Layout, SavedLayout};
use super::session::PaneId;

/// At most this many tabs: ⌘⌥1…⌘⌥9 reach each one.
pub const MAX_TABS: usize = 9;

/// The saved tabs for one target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub tabs: Vec<SavedLayout>,
    /// The tab shown (0-based).
    #[serde(default)]
    pub active: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Tab {
    layout: Layout,
    focused: PaneId,
}

impl Tab {
    fn new(layout: Layout) -> Tab {
        let focused = layout.panes()[0];
        Tab { layout, focused }
    }
}

/// The tabs, in strip order, and the one shown. Never empty.
#[derive(Clone, Debug, PartialEq)]
pub struct Tabs {
    tabs: Vec<Tab>,
    active: usize,
}

impl Default for Tabs {
    fn default() -> Self {
        Tabs::single()
    }
}

impl Tabs {
    /// One tab with one empty pane.
    pub fn single() -> Tabs {
        Tabs::from_layout(Layout::single())
    }

    /// One tab with `layout`.
    pub fn from_layout(layout: Layout) -> Tabs {
        Tabs {
            tabs: vec![Tab::new(layout)],
            active: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Never true; for clippy's `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The shown tab (0-based).
    pub fn active(&self) -> usize {
        self.active
    }

    /// The shown tab's layout.
    pub fn layout(&self) -> &Layout {
        &self.tabs[self.active].layout
    }

    pub fn layout_mut(&mut self) -> &mut Layout {
        &mut self.tabs[self.active].layout
    }

    /// Tab `i`'s layout.
    pub fn tab_layout(&self, i: usize) -> Option<&Layout> {
        self.tabs.get(i).map(|t| &t.layout)
    }

    /// The shown tab's focused pane.
    pub fn focused(&self) -> PaneId {
        self.tabs[self.active].focused
    }

    /// Tab `i`'s focused pane (the one focused when it is shown again).
    pub fn tab_focused(&self, i: usize) -> Option<PaneId> {
        self.tabs.get(i).map(|t| t.focused)
    }

    /// Focuses `pane` if the shown tab has it; `true` if the focus moved.
    pub fn set_focused(&mut self, pane: PaneId) -> bool {
        let tab = &mut self.tabs[self.active];
        if tab.focused == pane || !tab.layout.panes().contains(&pane) {
            return false;
        }
        tab.focused = pane;
        true
    }

    /// Every tab's panes: tab by tab, each in reading order.
    pub fn panes(&self) -> Vec<PaneId> {
        self.tabs.iter().flat_map(|t| t.layout.panes()).collect()
    }

    /// The tab holding `pane`.
    pub fn tab_of(&self, pane: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.layout.panes().contains(&pane))
    }

    /// The agent `pane` shows, in whichever tab.
    pub fn agent(&self, pane: PaneId) -> Option<u64> {
        let i = self.tab_of(pane)?;
        self.tabs[i].layout.agent(pane)
    }

    pub fn set_agent(&mut self, pane: PaneId, agent: Option<u64>) {
        if let Some(i) = self.tab_of(pane) {
            self.tabs[i].layout.set_agent(pane, agent);
        }
    }

    /// Every pane, in any tab, showing `old` shows `new` instead; returns
    /// those panes.
    pub fn replace_agent(&mut self, old: u64, new: Option<u64>) -> Vec<PaneId> {
        self.tabs
            .iter_mut()
            .flat_map(|t| t.layout.replace_agent(old, new))
            .collect()
    }

    /// The lowest pane id no tab has handed out.
    fn next_free(&self) -> u32 {
        self.tabs
            .iter()
            .map(|t| t.layout.next_id())
            .max()
            .unwrap_or(1)
    }

    /// Splits the shown tab's focused pane; the new pane (unique across
    /// tabs) takes the focus. `None` at the layout's pane limit.
    pub fn split(&mut self, axis: Axis) -> Option<PaneId> {
        let next = self.next_free();
        let tab = &mut self.tabs[self.active];
        tab.layout.reserve_ids(next);
        let new = tab.layout.split(tab.focused, axis)?;
        tab.focused = new;
        Some(new)
    }

    /// Closes the shown tab's focused pane; the focus goes to its sibling.
    /// The last pane stays, empty. Returns the closed pane.
    pub fn close_focused(&mut self) -> Option<PaneId> {
        let tab = &mut self.tabs[self.active];
        let pane = tab.focused;
        tab.focused = tab.layout.close(pane)?;
        Some(pane)
    }

    /// A new tab with one empty pane, right of the shown one, and shown.
    /// Returns its pane; `None` at [`MAX_TABS`].
    pub fn new_tab(&mut self) -> Option<PaneId> {
        if self.tabs.len() >= MAX_TABS {
            return None;
        }
        let pane = PaneId(self.next_free());
        self.active += 1;
        self.tabs
            .insert(self.active, Tab::new(Layout::single_with(pane)));
        Some(pane)
    }

    /// Closes tab `i`, returning its panes (to detach, never kill). The
    /// tab to its right is shown if `i` was, else the one to its left.
    /// The last tab stays: `None`, as for an unknown tab.
    pub fn close_tab(&mut self, i: usize) -> Option<Vec<PaneId>> {
        if self.tabs.len() <= 1 || i >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(i);
        if self.active > i || self.active == self.tabs.len() {
            self.active -= 1;
        }
        Some(tab.layout.panes())
    }

    /// Shows tab `i`; `true` if that changed what is shown.
    pub fn select(&mut self, i: usize) -> bool {
        if i >= self.tabs.len() || i == self.active {
            return false;
        }
        self.active = i;
        true
    }

    /// Shows the next tab, wrapping around.
    pub fn select_next(&mut self) -> bool {
        self.select((self.active + 1) % self.tabs.len())
    }

    /// Shows the previous tab, wrapping around.
    pub fn select_prev(&mut self) -> bool {
        let n = self.tabs.len();
        self.select((self.active + n - 1) % n)
    }

    pub fn to_saved(&self) -> SavedTabs {
        SavedTabs {
            tabs: self.tabs.iter().map(|t| t.layout.to_saved()).collect(),
            active: self.active,
        }
    }

    /// Rebuilds the tabs: at most [`MAX_TABS`]; a tab reusing a pane id of
    /// an earlier one is dropped; none left gives [`Tabs::single`]. An
    /// unknown `active` shows the first tab. Each tab focuses its first pane.
    pub fn from_saved(saved: &SavedTabs) -> Tabs {
        let mut tabs: Vec<Tab> = Vec::new();
        let mut active = 0;
        for (i, s) in saved.tabs.iter().enumerate() {
            if tabs.len() >= MAX_TABS {
                break;
            }
            let layout = Layout::from_saved(s);
            let clash = layout
                .panes()
                .iter()
                .any(|p| tabs.iter().any(|t| t.layout.panes().contains(p)));
            if clash {
                continue;
            }
            if i == saved.active {
                active = tabs.len();
            }
            tabs.push(Tab::new(layout));
        }
        if tabs.is_empty() {
            return Tabs::single();
        }
        Tabs { tabs, active }
    }
}

/// A tab's label in the strip: its number, then the program its focused
/// pane shows, if any.
pub fn tab_label(number: usize, program: Option<&str>) -> String {
    match program {
        Some(p) => format!("{number} {p}"),
        None => number.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(tabs: &Tabs) -> Vec<u32> {
        tabs.panes().iter().map(|p| p.0).collect()
    }

    #[test]
    fn starts_with_one_tab_of_one_pane() {
        let t = Tabs::single();
        assert_eq!((t.len(), t.active()), (1, 0));
        assert_eq!(t.focused(), PaneId(1));
        assert_eq!(ids(&t), [1]);
    }

    #[test]
    fn new_tab_goes_right_of_the_shown_one_with_a_unique_pane() {
        let mut t = Tabs::single();
        t.split(Axis::Horizontal).unwrap();
        let p = t.new_tab().unwrap();
        assert_eq!(p, PaneId(3));
        assert_eq!((t.len(), t.active(), t.focused()), (2, 1, p));
        assert_eq!(t.layout().panes(), [p]);
        t.select(0);
        let q = t.new_tab().unwrap();
        assert_eq!(q, PaneId(4));
        // Inserted at 1, before the tab holding pane 3.
        assert_eq!(t.active(), 1);
        assert_eq!(t.tab_layout(2).unwrap().panes(), [PaneId(3)]);
    }

    #[test]
    fn splits_in_any_tab_never_reuse_another_tabs_pane_id() {
        let mut t = Tabs::single();
        t.new_tab().unwrap(); // pane 2
        t.select(0);
        let a = t.split(Axis::Horizontal).unwrap();
        assert_eq!(a, PaneId(3));
        assert_eq!(t.focused(), a);
        t.select(1);
        let b = t.split(Axis::Vertical).unwrap();
        assert_eq!(b, PaneId(4));
        let mut all = ids(&t);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn each_tab_keeps_its_own_layout_and_focus() {
        let mut t = Tabs::single();
        t.split(Axis::Horizontal).unwrap(); // tab 0: 1 | 2, focus 2
        t.layout_mut()
            .move_divider(crate::core::layout::SplitId(0), 300.0, 800.0, 600.0, 10.0);
        t.new_tab().unwrap(); // tab 1: 3
        assert_eq!(t.layout().panes(), [PaneId(3)]);
        t.select(0);
        assert_eq!(t.focused(), PaneId(2));
        assert_eq!(t.layout().panes(), [PaneId(1), PaneId(2)]);
        let r = t.layout().ratio(crate::core::layout::SplitId(0)).unwrap();
        assert!((r - 0.375).abs() < 1e-4, "{r}");
    }

    #[test]
    fn set_focused_only_takes_a_pane_of_the_shown_tab() {
        let mut t = Tabs::single();
        t.new_tab().unwrap(); // pane 2, shown
        assert!(!t.set_focused(PaneId(1)));
        assert_eq!(t.focused(), PaneId(2));
        t.select(0);
        assert!(!t.set_focused(PaneId(1)), "already focused");
        t.split(Axis::Horizontal).unwrap(); // 3
        assert!(t.set_focused(PaneId(1)));
        assert_eq!(t.focused(), PaneId(1));
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut t = Tabs::single();
        assert!(
            !t.select_next() && !t.select_prev(),
            "one tab: nothing to switch to"
        );
        t.new_tab();
        t.new_tab();
        assert_eq!(t.active(), 2);
        assert!(t.select_next());
        assert_eq!(t.active(), 0);
        assert!(t.select_prev());
        assert_eq!(t.active(), 2);
        assert!(t.select_prev());
        assert_eq!(t.active(), 1);
    }

    #[test]
    fn select_ignores_unknown_and_current_tabs() {
        let mut t = Tabs::single();
        t.new_tab();
        assert!(!t.select(1));
        assert!(!t.select(5));
        assert!(t.select(0));
        assert_eq!(t.active(), 0);
    }

    #[test]
    fn tabs_stop_at_nine() {
        let mut t = Tabs::single();
        for _ in 1..MAX_TABS {
            assert!(t.new_tab().is_some());
        }
        assert_eq!(t.len(), MAX_TABS);
        assert_eq!(t.new_tab(), None);
    }

    #[test]
    fn close_tab_returns_its_panes_and_shows_a_neighbour() {
        let mut t = Tabs::single(); // [1]
        t.split(Axis::Horizontal); // [1 2]
        t.new_tab(); // [3]
        t.new_tab(); // [4]
        // Closing the shown middle tab shows the one to its right.
        t.select(1);
        assert_eq!(t.close_tab(1), Some(vec![PaneId(3)]));
        assert_eq!((t.len(), t.active(), t.focused()), (2, 1, PaneId(4)));
        // Closing the shown last tab shows the one to its left.
        assert_eq!(t.close_tab(1), Some(vec![PaneId(4)]));
        assert_eq!((t.len(), t.active()), (1, 0));
        // The last tab stays.
        assert_eq!(t.close_tab(0), None);
        assert_eq!(ids(&t), [1, 2]);
    }

    #[test]
    fn closing_a_tab_left_of_the_shown_one_keeps_it_shown() {
        let mut t = Tabs::single();
        t.new_tab(); // 2
        t.new_tab(); // 3, shown at 2
        assert_eq!(t.close_tab(0), Some(vec![PaneId(1)]));
        assert_eq!((t.active(), t.focused()), (1, PaneId(3)));
        assert_eq!(t.close_tab(7), None);
    }

    #[test]
    fn close_focused_moves_focus_within_the_tab() {
        let mut t = Tabs::single();
        let b = t.split(Axis::Horizontal).unwrap();
        assert_eq!(t.close_focused(), Some(b));
        assert_eq!(t.focused(), PaneId(1));
        // The last pane stays, empty.
        assert_eq!(t.close_focused(), Some(PaneId(1)));
        assert_eq!(t.layout().panes(), [PaneId(1)]);
    }

    #[test]
    fn agents_are_found_and_replaced_in_every_tab() {
        let mut t = Tabs::single();
        t.layout_mut().set_agent(PaneId(1), Some(7));
        t.new_tab(); // 2
        t.set_agent(PaneId(2), Some(7));
        t.split(Axis::Horizontal); // 3
        t.set_agent(PaneId(3), Some(8));
        assert_eq!(t.agent(PaneId(1)), Some(7));
        assert_eq!(t.tab_of(PaneId(3)), Some(1));
        assert_eq!(t.tab_of(PaneId(9)), None);
        assert_eq!(t.replace_agent(7, Some(9)), [PaneId(1), PaneId(2)]);
        assert_eq!((t.agent(PaneId(1)), t.agent(PaneId(2))), (Some(9), Some(9)));
        assert_eq!(t.agent(PaneId(3)), Some(8));
    }

    #[test]
    fn saved_tabs_round_trip() {
        let mut t = Tabs::single();
        t.layout_mut().set_agent(PaneId(1), Some(5));
        t.new_tab();
        t.split(Axis::Vertical);
        t.set_agent(PaneId(3), Some(6));
        t.new_tab();
        t.select(1);
        let saved = t.to_saved();
        assert_eq!(saved.active, 1);
        let back = Tabs::from_saved(&saved);
        assert_eq!(back.to_saved(), saved);
        assert_eq!(back.active(), 1);
        assert_eq!(back.agent(PaneId(3)), Some(6));
        // Ids stay unique after loading.
        let mut back = back;
        assert_eq!(back.new_tab(), Some(PaneId(5)));
    }

    #[test]
    fn saved_tabs_that_clash_or_overflow_are_dropped() {
        let one = Layout::single().to_saved();
        let saved = SavedTabs {
            tabs: vec![one.clone(), one.clone(), one],
            active: 2,
        };
        let t = Tabs::from_saved(&saved);
        assert_eq!((t.len(), t.active()), (1, 0));

        let mut many = Tabs::single();
        for _ in 1..MAX_TABS {
            many.new_tab();
        }
        let mut saved = many.to_saved();
        let extra = Layout::single_with(PaneId(50)).to_saved();
        saved.tabs.push(extra);
        saved.active = 99;
        let t = Tabs::from_saved(&saved);
        assert_eq!((t.len(), t.active()), (MAX_TABS, 0));

        let empty = SavedTabs {
            tabs: vec![],
            active: 0,
        };
        assert_eq!(Tabs::from_saved(&empty), Tabs::single());
    }

    #[test]
    fn a_saved_tab_focuses_its_first_pane() {
        let mut t = Tabs::single();
        t.split(Axis::Horizontal);
        assert_eq!(t.focused(), PaneId(2));
        let back = Tabs::from_saved(&t.to_saved());
        assert_eq!(back.focused(), PaneId(1));
        assert_eq!(back.tab_focused(0), Some(PaneId(1)));
        assert_eq!(back.tab_focused(1), None);
    }

    #[test]
    fn labels_number_the_tab_then_name_its_program() {
        assert_eq!(tab_label(1, None), "1");
        assert_eq!(tab_label(3, Some("claude")), "3 claude");
    }
}
