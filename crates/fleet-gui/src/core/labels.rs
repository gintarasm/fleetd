//! Editing an agent's labels after it started (issue #99, ADR 0087): the
//! Edit Labels sheet's rows, what they change, and the sidebar chip that
//! filters by its label.

use std::collections::HashMap;

use fleet_client::names::NAME_LABEL;

/// Label keys the core keeps for itself (ADR 0025); fleetd refuses to set
/// or remove them.
pub const RESERVED_PREFIX: &str = "fleet.";

/// The key is shown in the Edit Labels sheet but not edited there: the
/// core's `fleet.*` labels, and the name (which Rename changes).
pub fn read_only(key: &str) -> bool {
    key == NAME_LABEL || key.starts_with(RESERVED_PREFIX)
}

/// The labels the sheet lists as editable rows, sorted by key.
pub fn editable_labels(labels: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<_> = labels
        .iter()
        .filter(|(k, _)| !read_only(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.sort();
    out
}

/// The labels the sheet shows read-only: the name first, then the
/// core's, sorted.
pub fn read_only_labels(labels: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<_> = labels
        .iter()
        .filter(|(k, _)| read_only(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.sort_by_key(|(k, _)| (k != NAME_LABEL, k.clone()));
    out
}

/// One `key=value` item, trimmed around the key and the value. The key
/// must not be empty or the core's; the value may be.
pub fn parse_label(item: &str) -> Result<(String, String), String> {
    let (k, v) = match item.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => (k.trim(), v.trim()),
        _ => return Err(format!("label {item:?}: expected key=value")),
    };
    if k.starts_with(RESERVED_PREFIX) {
        return Err(format!(
            "label {k:?}: {RESERVED_PREFIX}* labels are fleetd's"
        ));
    }
    Ok((k.to_owned(), v.to_owned()))
}

/// What a save asks fleetd for: one `SetLabels` (set, then remove).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelEdit {
    pub set: HashMap<String, String>,
    /// Sorted.
    pub remove: Vec<String>,
}

impl LabelEdit {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// The change from the agent's labels `old` to the sheet's `rows`
/// (`key=value` each; blank rows are skipped): the labels added or
/// changed, and the editable ones no row keeps. A key given twice, the
/// name or a `fleet.*` key is an error. Read-only labels are never
/// removed.
pub fn label_edit<S: AsRef<str>>(
    old: &HashMap<String, String>,
    rows: &[S],
) -> Result<LabelEdit, String> {
    let mut new = HashMap::new();
    for row in rows.iter().map(|r| r.as_ref().trim()) {
        if row.is_empty() {
            continue;
        }
        let (k, v) = parse_label(row)?;
        if k == NAME_LABEL {
            return Err("change the name with Rename…".into());
        }
        if new.insert(k.clone(), v).is_some() {
            return Err(format!("label {k:?} is given twice"));
        }
    }
    let set = new
        .iter()
        .filter(|(k, v)| old.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut remove: Vec<String> = old
        .keys()
        .filter(|k| !read_only(k) && !new.contains_key(*k))
        .cloned()
        .collect();
    remove.sort();
    Ok(LabelEdit { set, remove })
}

/// The sidebar filter after a click on the chip `key=value`: the chip's
/// word is added to the filter's words, or taken out if it is there
/// already (a second click undoes the first).
pub fn chip_filter(query: &str, key: &str, value: &str) -> String {
    let token = format!("{key}={value}");
    let words: Vec<&str> = query.split_whitespace().collect();
    let lower = token.to_lowercase();
    let kept: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| w.to_lowercase() != lower)
        .collect();
    if kept.len() == words.len() {
        let mut out = kept;
        out.push(&token);
        out.join(" ")
    } else {
        kept.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_sheet_splits_editable_and_read_only_labels() {
        let l = labels(&[
            ("role", "reviewer"),
            ("name", "otter"),
            ("fleet.parent", "3"),
            ("project", "fleetd"),
            ("fleet.depth", "1"),
        ]);
        assert_eq!(
            editable_labels(&l),
            [
                ("project".to_owned(), "fleetd".to_owned()),
                ("role".to_owned(), "reviewer".to_owned())
            ]
        );
        let ro: Vec<_> = read_only_labels(&l).into_iter().map(|(k, _)| k).collect();
        assert_eq!(ro, ["name", "fleet.depth", "fleet.parent"]);
    }

    #[test]
    fn parse_label_trims_and_refuses_the_cores_keys() {
        assert_eq!(
            parse_label(" role = reviewer "),
            Ok(("role".into(), "reviewer".into()))
        );
        assert_eq!(parse_label("k="), Ok(("k".into(), String::new())));
        assert_eq!(parse_label("k=a=b"), Ok(("k".into(), "a=b".into())));
        assert!(parse_label("role").is_err());
        assert!(parse_label(" =x").is_err());
        assert!(parse_label("fleet.parent=1").is_err());
    }

    #[test]
    fn label_edit_sets_what_changed_and_removes_what_went() {
        let old = labels(&[
            ("role", "coder"),
            ("team", "core"),
            ("gone", "x"),
            ("name", "otter"),
            ("fleet.parent", "3"),
        ]);
        let e = label_edit(&old, &["role=reviewer", "team=core", "", "new=1"]).unwrap();
        assert_eq!(e.set, labels(&[("role", "reviewer"), ("new", "1")]));
        // Neither the name nor the core's labels are removed.
        assert_eq!(e.remove, ["gone"]);
        // Nothing changed: nothing to send.
        let same = label_edit(&old, &["gone=x", "team=core", "role=coder"]).unwrap();
        assert!(same.is_empty());
    }

    #[test]
    fn label_edit_refuses_bad_rows() {
        let old = HashMap::new();
        assert!(label_edit(&old, &["role"]).is_err());
        assert!(label_edit(&old, &["name=x"]).is_err());
        assert!(label_edit(&old, &["fleet.depth=2"]).is_err());
        let twice = label_edit(&old, &["a=1", " a = 2"]).unwrap_err();
        assert!(twice.contains("twice"), "{twice}");
    }

    #[test]
    fn a_chip_adds_its_label_to_the_filter_and_a_second_click_takes_it_out() {
        assert_eq!(chip_filter("", "role", "reviewer"), "role=reviewer");
        assert_eq!(
            chip_filter("claude", "role", "reviewer"),
            "claude role=reviewer"
        );
        assert_eq!(
            chip_filter("claude Role=Reviewer", "role", "reviewer"),
            "claude"
        );
        assert_eq!(chip_filter("  role=reviewer  ", "role", "reviewer"), "");
    }
}
