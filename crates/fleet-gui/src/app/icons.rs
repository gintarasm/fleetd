//! The vendored Lucide icons ([`Icon`]) as gpui assets, compiled into the
//! binary. gpui parses and rasterises an icon the first time it is drawn
//! at a size and keeps the result in its sprite atlas, so nothing is
//! decoded at launch.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, svg};

use crate::core::icons::Icon;
use crate::core::style::chrome;

fn bytes(icon: Icon) -> &'static [u8] {
    macro_rules! file {
        ($name:literal) => {
            include_bytes!(concat!("../../assets/icons/", $name, ".svg"))
        };
    }
    match icon {
        Icon::Plus => file!("plus"),
        Icon::Close => file!("x"),
        Icon::SplitRight => file!("columns-2"),
        Icon::SplitDown => file!("rows-2"),
        Icon::Maximize => file!("maximize-2"),
        Icon::Restore => file!("minimize-2"),
        Icon::Restart => file!("rotate-ccw"),
        Icon::Trash => file!("trash-2"),
        Icon::Terminal => file!("terminal"),
        Icon::Server => file!("server"),
        Icon::Folder => file!("folder-open"),
        Icon::Stop => file!("circle-stop"),
        Icon::Check => file!("check"),
        Icon::ChevronUp => file!("chevron-up"),
        Icon::ChevronDown => file!("chevron-down"),
        Icon::Graph => file!("network"),
    }
}

/// The app's asset source: the icons.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Icon::from_path(path).map(|i| Cow::Borrowed(bytes(i))))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .into_iter()
            .map(Icon::path)
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

/// `icon` at the toolbar size, in the text colour.
pub fn icon(icon: Icon) -> Svg {
    sized(icon, chrome::ICON)
}

/// `icon` at `size` px square, in the text colour.
pub fn sized(icon: Icon, size: f32) -> Svg {
    svg().path(icon.path()).flex_none().size(px(size))
}

#[cfg(test)]
mod tests {
    use super::Assets;
    use crate::core::icons::Icon;
    use gpui::AssetSource;

    #[test]
    fn every_icon_loads() {
        for icon in Icon::ALL {
            let data = Assets.load(icon.path()).unwrap().expect("compiled in");
            assert!(std::str::from_utf8(&data).unwrap().contains("<svg"));
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
        assert_eq!(Assets.list("icons/").unwrap().len(), Icon::ALL.len());
    }
}
