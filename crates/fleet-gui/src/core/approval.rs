//! Spawn approval in the GUI (ADR 0079): what a pending spawn's card in the
//! sidebar says. gpui-free.

use fleet_proto::PendingSpawn;

/// A pending spawn's card text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingText {
    /// Who asks: "lead (7) wants to start an agent".
    pub title: String,
    /// The command line, shell-quoted where needed.
    pub command: String,
    pub cwd: String,
    /// The child's labels as `k=v`, sorted, without `fleet.*` (the parent
    /// is in the title).
    pub labels: Vec<String>,
}

/// The card text of `p`, asked by the agent named `asker` (`None` when the
/// parent is not listed).
pub fn pending_text(p: &PendingSpawn, asker: Option<&str>) -> PendingText {
    let title = match asker {
        Some(name) => format!("{name} ({}) wants to start an agent", p.parent),
        None => format!("Agent {} wants to start an agent", p.parent),
    };
    let command = std::iter::once(p.program.as_str())
        .chain(p.args.iter().map(String::as_str))
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut labels: Vec<String> = p
        .labels
        .iter()
        .filter(|(k, _)| !k.starts_with("fleet."))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    labels.sort();
    PendingText {
        title,
        command,
        cwd: p.cwd.clone(),
        labels,
    }
}

/// `arg` quoted when a shell would need it (a space, a quote, empty).
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\'', '"', '\t', '\n']) {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_names_the_asker_the_command_and_the_labels() {
        let p = PendingSpawn {
            id: 1,
            parent: 7,
            program: "claude".into(),
            args: vec!["-p".into(), "it's done".into()],
            cwd: "/src".into(),
            labels: [("fleet.parent", "7"), ("team", "x"), ("name", "kid")]
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            pending_text(&p, Some("lead")),
            PendingText {
                title: "lead (7) wants to start an agent".into(),
                command: r"claude -p 'it'\''s done'".into(),
                cwd: "/src".into(),
                labels: vec!["name=kid".into(), "team=x".into()],
            }
        );
        assert_eq!(
            pending_text(&p, None).title,
            "Agent 7 wants to start an agent"
        );
    }
}
