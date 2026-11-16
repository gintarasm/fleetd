//! Fuzzy matching: does a query's characters appear, in order, in a
//! string, and how well. Case-insensitive; spaces in the query are
//! ignored. The score favours matches at word starts and runs of
//! consecutive characters, and penalises gaps between matched characters.

/// Each matched character.
const MATCH: i32 = 16;
/// A matched character at a word start (after a space, `/`, `-`, `_`,
/// `.`, or a lower-to-upper case change).
const BOUNDARY: i32 = 10;
/// A matched character at the very start of the string.
const START: i32 = 12;
/// A matched character right after the previous matched one.
const CONSECUTIVE: i32 = 8;
/// Per character skipped between two matched characters.
const GAP: i32 = 1;

/// A match: its score (higher is better) and the byte offset of each
/// matched character in the string, ascending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Matches `query` against `text`; `None` when some query character is
/// missing (in order). An empty query matches everything with score 0.
pub fn fuzzy_match(query: &str, text: &str) -> Option<Match> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if q.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let raw: Vec<(usize, char)> = text.char_indices().collect();
    let t: Vec<char> = raw.iter().map(|&(_, c)| fold(c)).collect();
    let (n, m) = (q.len(), t.len());
    if n > m {
        return None;
    }
    let bonus: Vec<i32> = (0..m)
        .map(|j| match j {
            0 => START,
            _ if is_boundary(raw[j - 1].1, raw[j].1) => BOUNDARY,
            _ => 0,
        })
        .collect();
    // best[i][j]: the best score with q[i] matched at t[j] (None: cannot);
    // from[i][j]: where q[i - 1] was matched for it.
    let mut best = vec![vec![None::<i32>; m]; n];
    let mut from = vec![vec![0usize; m]; n];
    for j in 0..m {
        if t[j] == q[0] {
            best[0][j] = Some(MATCH + bonus[j]);
        }
    }
    for i in 1..n {
        // The best `best[i - 1][k] + k * GAP` over k < j - 1, and its k.
        let mut far: Option<(i32, usize)> = None;
        for j in i..m {
            if j >= 2
                && let Some(s) = best[i - 1][j - 2]
            {
                let v = s + (j as i32 - 2) * GAP;
                if far.is_none_or(|(f, _)| v > f) {
                    far = Some((v, j - 2));
                }
            }
            if t[j] != q[i] {
                continue;
            }
            let near = best[i - 1][j - 1].map(|s| (s + CONSECUTIVE, j - 1));
            let gapped = far.map(|(v, k)| (v - (j as i32 - 1) * GAP, k));
            let pick = match (near, gapped) {
                (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
                (a, b) => a.or(b),
            };
            if let Some((s, k)) = pick {
                best[i][j] = Some(s + MATCH + bonus[j]);
                from[i][j] = k;
            }
        }
    }
    // The first (leftmost) of the best ends.
    let (mut j, score) = best[n - 1]
        .iter()
        .enumerate()
        .filter_map(|(j, s)| s.map(|s| (j, s)))
        .fold(None::<(usize, i32)>, |acc, (j, s)| match acc {
            Some((_, b)) if b >= s => acc,
            _ => Some((j, s)),
        })?;
    let mut positions = vec![0; n];
    for i in (0..n).rev() {
        positions[i] = raw[j].0;
        j = from[i][j];
    }
    Some(Match { score, positions })
}

fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

fn is_boundary(prev: char, c: char) -> bool {
    matches!(prev, ' ' | '/' | '-' | '_' | '.' | ':' | '=' | '·')
        || (prev.is_lowercase() && c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, t: &str) -> i32 {
        fuzzy_match(q, t)
            .unwrap_or_else(|| panic!("{q:?} matches {t:?}"))
            .score
    }

    fn positions(q: &str, t: &str) -> Vec<usize> {
        fuzzy_match(q, t).unwrap().positions
    }

    #[test]
    fn characters_must_appear_in_order() {
        assert!(fuzzy_match("spr", "Split Right").is_some());
        assert!(fuzzy_match("rps", "Split Right").is_none());
        assert!(fuzzy_match("x", "Split Right").is_none());
        assert!(fuzzy_match("toolong", "tool").is_none());
    }

    #[test]
    fn case_and_spaces_in_the_query_are_ignored() {
        assert_eq!(positions("SR", "split right"), [0, 6]);
        assert_eq!(positions("split r", "Split Right"), [0, 1, 2, 3, 4, 6]);
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(
            fuzzy_match("  ", "anything"),
            Some(Match {
                score: 0,
                positions: vec![]
            })
        );
    }

    #[test]
    fn word_starts_beat_letters_inside_words() {
        // "sd": Split Down's initials, not the s and d of "Pastes old".
        assert!(score("sd", "Split Down") > score("sd", "Pastes old"));
        assert_eq!(positions("sd", "Split Down"), [0, 6]);
        // Path components and camel case count as words.
        assert_eq!(positions("fg", "~/src/fleet-gui"), [6, 12]);
        assert_eq!(positions("ns", "newShell"), [0, 3]);
    }

    #[test]
    fn runs_beat_scattered_letters() {
        assert!(score("term", "New terminal") > score("term", "the error mode"));
        assert_eq!(positions("term", "New terminal"), [4, 5, 6, 7]);
    }

    #[test]
    fn a_prefix_beats_the_same_word_later() {
        assert!(score("new", "New agent") > score("new", "Open new split"));
    }

    #[test]
    fn shorter_gaps_beat_longer_ones() {
        assert!(score("ab", "a-b") > score("ab", "a--b"));
        assert!(score("ac", "abc") > score("ac", "abbbbc"));
    }

    #[test]
    fn the_best_alignment_is_found_not_the_first() {
        // Greedy matching would take the first `r` (in "Split") and lose
        // the run in "Right".
        assert_eq!(positions("right", "Split Right"), [6, 7, 8, 9, 10]);
    }

    #[test]
    fn positions_are_byte_offsets() {
        // "é" is two bytes.
        assert_eq!(positions("éx", "éx"), [0, 2]);
        assert_eq!(positions("x", "é x"), [3]);
        assert_eq!(positions("é", "café"), [3]);
    }
}
