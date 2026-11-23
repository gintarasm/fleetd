//! Search All Agents (⌘⇧F, ADR 0098): what the picker asks the node and
//! how it shows the hits. The node does the searching (`SearchOutput`);
//! this decides when to ask (debounced, one request at a time, the latest
//! query) and turns hits into picker rows grouped by agent.

use std::path::Path;
use std::time::Duration;

use fleet_proto::SearchHit;

use super::find::Needle;
use super::palette::agent_item;
use super::picker::PickerItem;
use super::session::AgentRow;

/// How long typing must pause before the node is asked.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// Hits asked for in all, and per agent.
pub const LIMIT: u32 = 200;
pub const PER_AGENT: u32 = 20;
/// A row's text at most, in characters.
pub const SNIPPET_CHARS: usize = 120;
/// Characters kept before the match when the line is cut.
const LEAD_CHARS: usize = 30;

/// Case sensitive only when the query has an upper-case letter (smart
/// case): `error` finds `Error`, `Error` does not find `error`.
pub fn smart_case(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// The query typed and the request in flight: at most one at a time, and
/// a query typed meanwhile is asked for once the answer arrives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requests {
    latest: String,
    in_flight: Option<String>,
    /// The query the shown hits answer.
    shown: Option<String>,
}

impl Requests {
    /// The query changed (start the debounce, then call [`Requests::to_send`]).
    pub fn set_query(&mut self, query: &str) {
        self.latest = query.to_owned();
        if query.is_empty() {
            self.shown = None;
        }
    }

    pub fn query(&self) -> &str {
        &self.latest
    }

    /// The query to send now, if any: none while one is in flight, for an
    /// empty query, or when the hits shown already answer it.
    pub fn to_send(&mut self) -> Option<String> {
        if self.in_flight.is_some()
            || self.latest.is_empty()
            || self.shown.as_deref() == Some(self.latest.as_str())
        {
            return None;
        }
        self.in_flight = Some(self.latest.clone());
        self.in_flight.clone()
    }

    /// The answer to `query` arrived: `true` if it is to be shown (the
    /// query was not cleared meanwhile). Call [`Requests::to_send`] after, for
    /// a query typed while it ran.
    pub fn answered(&mut self, query: &str) -> bool {
        self.in_flight = None;
        if self.latest.is_empty() {
            return false;
        }
        self.shown = Some(query.to_owned());
        true
    }

    /// A request is running.
    pub fn searching(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// The picker's rows for `hits` (grouped by agent as the node orders
/// them): each row's key, title (the line around its first match), the
/// title's matched byte offsets, and the agent's heading on the first row
/// of each agent.
pub struct HitRows {
    pub items: Vec<PickerItem>,
    pub hits: Vec<Vec<usize>>,
    pub sections: Vec<Option<String>>,
}

pub fn hit_rows(hits: &[SearchHit], agents: &[AgentRow], needle: &Needle, home: &Path) -> HitRows {
    let mut out = HitRows {
        items: Vec::with_capacity(hits.len()),
        hits: Vec::with_capacity(hits.len()),
        sections: Vec::with_capacity(hits.len()),
    };
    let mut last = None;
    for h in hits {
        let section = (last != Some(h.agent_id)).then(|| {
            match agents.iter().find(|r| r.info.id == h.agent_id) {
                Some(row) => {
                    let item = agent_item(row, home);
                    format!("{} — {}", item.title, item.detail)
                }
                None => format!("Agent {}", h.agent_id),
            }
        });
        last = Some(h.agent_id);
        let title = snippet(&h.text, needle, SNIPPET_CHARS);
        out.hits.push(char_offsets(&title, needle));
        out.items.push(PickerItem {
            key: hit_key(h.agent_id, h.line),
            title,
            ..PickerItem::default()
        });
        out.sections.push(section);
    }
    out
}

/// A hit row's key.
pub fn hit_key(agent_id: u64, line: u64) -> String {
    format!("hit:{agent_id}:{line}")
}

/// The agent and line of a hit row's key.
pub fn parse_hit_key(key: &str) -> Option<(u64, u64)> {
    let (agent, line) = key.strip_prefix("hit:")?.split_once(':')?;
    Some((agent.parse().ok()?, line.parse().ok()?))
}

/// `text` without leading blanks, cut to `max` characters so the first
/// match shows: a line whose match lies far in starts `…` a little before
/// it, and a cut end gets `…`.
pub fn snippet(text: &str, needle: &Needle, max: usize) -> String {
    let text = text.trim_start();
    let at = needle
        .find_in(text)
        .map_or(0, |r| text[..r.start].chars().count());
    let (lead, start) = if at > LEAD_CHARS && text.chars().count() > max {
        ("…", at - LEAD_CHARS)
    } else {
        ("", 0)
    };
    let mut out: String = lead.to_owned();
    let mut chars = text.chars().skip(start);
    let room = max - lead.chars().count();
    out.extend(chars.by_ref().take(room));
    if chars.next().is_some() {
        out.pop();
        out.push('…');
    }
    out
}

/// The byte offset of every character inside a match of `needle` in
/// `text` (what the picker highlights).
fn char_offsets(text: &str, needle: &Needle) -> Vec<usize> {
    needle
        .find_all(text)
        .into_iter()
        .flat_map(|r| {
            text[r.clone()]
                .char_indices()
                .map(move |(i, _)| r.start + i)
        })
        .collect()
}

/// The text under the list: why it is empty, or that it is cut short.
pub fn status_text(query: &str, searching: bool, rows: usize, truncated: bool) -> Option<String> {
    if query.is_empty() {
        return Some("Type to search every agent's output".to_owned());
    }
    match (rows, searching, truncated) {
        (0, true, _) => Some("Searching…".to_owned()),
        (0, false, _) => Some("No matches".to_owned()),
        (_, _, true) => Some(format!("First {rows} matches; type more to narrow")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::AgentInfo;

    fn needle(q: &str) -> Needle {
        Needle::new(q, false).unwrap()
    }

    fn hit(agent_id: u64, line: u64, text: &str) -> SearchHit {
        SearchHit {
            agent_id,
            line,
            text: text.into(),
        }
    }

    fn row(id: u64, name: &str) -> AgentRow {
        let mut info = AgentInfo {
            id,
            program: "claude".into(),
            cwd: "/home/u/p".into(),
            ..AgentInfo::default()
        };
        info.labels.insert("name".into(), name.into());
        AgentRow {
            info,
            last_line: String::new(),
            last_output_unix_ms: None,
        }
    }

    #[test]
    fn one_request_at_a_time_and_the_latest_query_last() {
        let mut r = Requests::default();
        assert_eq!(r.to_send(), None, "nothing typed");
        r.set_query("er");
        assert_eq!(r.to_send().as_deref(), Some("er"));
        assert!(r.searching());
        r.set_query("err");
        r.set_query("erro");
        assert_eq!(r.to_send(), None, "one in flight");
        assert!(r.answered("er"));
        assert_eq!(r.to_send().as_deref(), Some("erro"), "only the latest");
        assert!(r.answered("erro"));
        assert_eq!(r.to_send(), None, "already shown");
        assert!(!r.searching());
    }

    #[test]
    fn a_cleared_query_drops_the_answer_in_flight() {
        let mut r = Requests::default();
        r.set_query("x");
        r.to_send();
        r.set_query("");
        assert!(!r.answered("x"));
        assert_eq!(r.to_send(), None);
        // Typing the same again asks again.
        r.set_query("x");
        assert_eq!(r.to_send().as_deref(), Some("x"));
    }

    #[test]
    fn rows_are_grouped_under_their_agent() {
        let hits = [
            hit(9, 40, "  build ERROR in x"),
            hit(9, 3, "error: y"),
            hit(4, 7, "no Error here"),
            hit(77, 0, "error gone"),
        ];
        let agents = [row(9, "builder"), row(4, "tester")];
        let rows = hit_rows(&hits, &agents, &needle("error"), Path::new("/home/u"));
        let titles: Vec<&str> = rows.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "build ERROR in x",
                "error: y",
                "no Error here",
                "error gone"
            ]
        );
        assert_eq!(rows.items[0].key, "hit:9:40");
        assert_eq!(rows.hits[0], vec![6, 7, 8, 9, 10]);
        assert_eq!(
            rows.sections,
            [
                Some("builder — ~/p · lost".to_owned()),
                None,
                Some("tester — ~/p · lost".to_owned()),
                Some("Agent 77".to_owned()),
            ]
        );
    }

    #[test]
    fn upper_case_makes_the_search_case_sensitive() {
        assert!(!smart_case("error"));
        assert!(smart_case("Error"));
        assert!(!smart_case("12:34 ž"));
        assert!(smart_case("Ž"));
    }

    #[test]
    fn keys_round_trip() {
        assert_eq!(parse_hit_key(&hit_key(12, 3456)), Some((12, 3456)));
        assert_eq!(parse_hit_key("agent:12"), None);
        assert_eq!(parse_hit_key("hit:x:1"), None);
    }

    #[test]
    fn snippets_show_the_match() {
        let n = needle("needle");
        assert_eq!(snippet("   short needle", &n, 20), "short needle");
        let long = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let s = snippet(&long, &n, 50);
        assert_eq!(s.chars().count(), 50);
        assert!(s.starts_with("…aaa"), "{s}");
        assert!(s.ends_with("b…"), "{s}");
        assert_eq!(s.find("needle"), Some("…".len() + 30));
        // Near the start: cut at the end only.
        let s = snippet(&format!("x needle {}", "c".repeat(100)), &n, 20);
        assert_eq!(s, format!("x needle {}…", "c".repeat(10)));
        // Multi-byte characters are cut whole.
        let s = snippet(&format!("{}needle", "界".repeat(60)), &n, 40);
        assert!(s.starts_with('…') && s.ends_with("needle"), "{s}");
    }

    #[test]
    fn status_explains_an_empty_or_cut_list() {
        assert_eq!(
            status_text("", false, 0, false).as_deref(),
            Some("Type to search every agent's output")
        );
        assert_eq!(
            status_text("x", true, 0, false).as_deref(),
            Some("Searching…")
        );
        assert_eq!(
            status_text("x", false, 0, false).as_deref(),
            Some("No matches")
        );
        assert_eq!(status_text("x", false, 3, false), None);
        assert_eq!(
            status_text("x", false, 200, true).as_deref(),
            Some("First 200 matches; type more to narrow")
        );
    }
}
