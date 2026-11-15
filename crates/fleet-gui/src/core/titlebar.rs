//! What the unified bar at the top of the window says: the fleetd it is
//! connected to, and how that connection and its agents are doing.

use super::target::Target;

/// The bar's two lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarTitle {
    /// The machine: "This Mac", or the remote host.
    pub title: String,
    /// The connection while it is not up, else the agent counts.
    pub subtitle: String,
}

/// The connection as the bar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// No session yet (the startup connect or the connect sheet).
    Connecting,
    Live,
    Reconnecting,
}

/// The bar's lines for `target`, with `running` and `ended` agents.
pub fn bar_title(target: &Target, link: Link, running: usize, ended: usize) -> BarTitle {
    let title = match target {
        Target::Local => "This Mac".to_owned(),
        Target::Ssh { dest } => dest
            .rsplit_once('@')
            .map_or(dest.as_str(), |(_, host)| host)
            .to_owned(),
    };
    let subtitle = match link {
        Link::Connecting => "connecting…".to_owned(),
        Link::Reconnecting => "reconnecting…".to_owned(),
        Link::Live => match (running, ended) {
            (0, 0) => "no agents".to_owned(),
            (r, 0) => format!("{r} running"),
            (0, e) => format!("{e} exited"),
            (r, e) => format!("{r} running · {e} exited"),
        },
    };
    BarTitle { title, subtitle }
}

/// The window's title (the Window menu, Mission Control): the bar's
/// title after the app's name.
pub fn window_title(bar: &BarTitle) -> String {
    format!("fleet — {}", bar.title)
}

#[cfg(test)]
mod tests {
    use super::{BarTitle, Link, bar_title, window_title};
    use crate::core::target::Target;

    fn ssh(dest: &str) -> Target {
        Target::Ssh { dest: dest.into() }
    }

    #[test]
    fn title_is_the_machine() {
        assert_eq!(
            bar_title(&Target::Local, Link::Live, 0, 0).title,
            "This Mac"
        );
        assert_eq!(bar_title(&ssh("me@box"), Link::Live, 0, 0).title, "box");
        assert_eq!(
            bar_title(&ssh("box.lan"), Link::Live, 0, 0).title,
            "box.lan"
        );
    }

    #[test]
    fn subtitle_counts_agents_while_live() {
        let sub = |r, e| bar_title(&Target::Local, Link::Live, r, e).subtitle;
        assert_eq!(sub(0, 0), "no agents");
        assert_eq!(sub(3, 0), "3 running");
        assert_eq!(sub(0, 2), "2 exited");
        assert_eq!(sub(3, 2), "3 running · 2 exited");
    }

    #[test]
    fn subtitle_shows_the_connection_while_down() {
        let t = ssh("me@box");
        assert_eq!(
            bar_title(&t, Link::Connecting, 4, 1).subtitle,
            "connecting…"
        );
        assert_eq!(
            bar_title(&t, Link::Reconnecting, 4, 1).subtitle,
            "reconnecting…"
        );
    }

    #[test]
    fn window_title_names_the_app() {
        let bar = BarTitle {
            title: "box".into(),
            subtitle: String::new(),
        };
        assert_eq!(window_title(&bar), "fleet — box");
    }
}
