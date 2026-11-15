//! The icons the GUI draws: Lucide icons (<https://lucide.dev>,
//! `lucide-static` 0.460.0, ISC license with MIT portions from Feather;
//! see `assets/icons/LICENSE`), vendored unmodified as SVG under
//! `crates/fleet-gui/assets/icons`. Only these are vendored.

/// One vendored icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Plus,
    Close,
    SplitRight,
    SplitDown,
    Maximize,
    Restore,
    Restart,
    Trash,
    Terminal,
    Server,
    Folder,
    /// Kill an agent: not the pane's Close (×), which only detaches.
    Stop,
    Check,
}

impl Icon {
    pub const ALL: [Icon; 13] = [
        Icon::Plus,
        Icon::Close,
        Icon::SplitRight,
        Icon::SplitDown,
        Icon::Maximize,
        Icon::Restore,
        Icon::Restart,
        Icon::Trash,
        Icon::Terminal,
        Icon::Server,
        Icon::Folder,
        Icon::Stop,
        Icon::Check,
    ];

    /// The Lucide name, which is the file's stem.
    pub fn lucide_name(self) -> &'static str {
        match self {
            Icon::Plus => "plus",
            Icon::Close => "x",
            Icon::SplitRight => "columns-2",
            Icon::SplitDown => "rows-2",
            Icon::Maximize => "maximize-2",
            Icon::Restore => "minimize-2",
            Icon::Restart => "rotate-ccw",
            Icon::Trash => "trash-2",
            Icon::Terminal => "terminal",
            Icon::Server => "server",
            Icon::Folder => "folder-open",
            Icon::Stop => "circle-stop",
            Icon::Check => "check",
        }
    }

    /// The asset path the shell loads it by: `icons/<name>.svg`.
    pub fn path(self) -> &'static str {
        match self {
            Icon::Plus => "icons/plus.svg",
            Icon::Close => "icons/x.svg",
            Icon::SplitRight => "icons/columns-2.svg",
            Icon::SplitDown => "icons/rows-2.svg",
            Icon::Maximize => "icons/maximize-2.svg",
            Icon::Restore => "icons/minimize-2.svg",
            Icon::Restart => "icons/rotate-ccw.svg",
            Icon::Trash => "icons/trash-2.svg",
            Icon::Terminal => "icons/terminal.svg",
            Icon::Server => "icons/server.svg",
            Icon::Folder => "icons/folder-open.svg",
            Icon::Stop => "icons/circle-stop.svg",
            Icon::Check => "icons/check.svg",
        }
    }

    /// The icon for an asset path, if it is one of ours.
    pub fn from_path(path: &str) -> Option<Icon> {
        Icon::ALL.into_iter().find(|i| i.path() == path)
    }
}

#[cfg(test)]
mod tests {
    use super::Icon;
    use std::path::Path;

    #[test]
    fn every_icon_is_vendored_with_the_license() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        for icon in Icon::ALL {
            assert_eq!(icon.path(), format!("icons/{}.svg", icon.lucide_name()));
            let svg = std::fs::read_to_string(dir.join(icon.path()))
                .unwrap_or_else(|e| panic!("{icon:?}: {e}"));
            assert!(
                svg.contains("lucide-static") && svg.contains("<svg"),
                "{icon:?}"
            );
            // Stroked in currentColor: drawn in the text colour.
            assert!(svg.contains("stroke=\"currentColor\""), "{icon:?}");
            assert_eq!(Icon::from_path(icon.path()), Some(icon));
        }
        let license = std::fs::read_to_string(dir.join("icons/LICENSE")).unwrap();
        assert!(license.contains("ISC License") && license.contains("Feather (MIT)"));
        assert_eq!(Icon::from_path("icons/nope.svg"), None);
    }

    #[test]
    fn only_listed_icons_are_vendored() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
        let mut svgs = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if let Some(stem) = name.strip_suffix(".svg") {
                svgs += 1;
                assert!(
                    Icon::ALL.iter().any(|i| i.lucide_name() == stem),
                    "{name} is vendored but unused"
                );
            }
        }
        assert_eq!(svgs, Icon::ALL.len());
    }
}
