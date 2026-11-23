# 0104 — macOS app icon: Icon Composer bundle for macOS 26, .icns fallback

**Status:** accepted · 2026-11-23 (issue #167)

Changes the icon step of the packaging in [0028](0028-gui-client.md). The menu-bar mark and the wordmark are unchanged.

## Context

In the Dock on macOS 26 the icon was a dark rounded square inside a larger white one. Two causes:

- `assets/logo/fleetd-icon.svg` draws the artwork as an 824×824 rounded rect (`rx=185`) inset 100 px on a 1024 canvas (the Big Sur grid). `scripts/bundle-macos.sh` rasterised it with `qlmanage -t`, which fills transparency white, so the shipped `fleetd.icns` had an opaque white margin (corner pixel RGBA 255,254,255,255; about 400k near-white pixels at 1024 px).
- macOS 26 draws every app icon in its own rounded-square shape with Liquid Glass. An app with only an `.icns` gets it scaled down inside a light "container" squircle, even when the `.icns` is clean, because the system cannot tell the art's shape. Apple documents no way for a plain `.icns` (full-bleed or not) to opt out of that, so we did not rely on one. The supported way is an asset catalog icon: an Icon Composer `.icon` bundle compiled by `actool` into `Assets.car`, named by `CFBundleIconName`.

## Decision

- **macOS 26: `assets/logo/fleetd.icon`**, an Icon Composer bundle (plain files: `icon.json` plus `Assets/glyph.svg`). The background is the `fill` linear gradient in `icon.json` (`#1b2433` to `#0c1119`, the old rect's gradient); the one layer, `glyph.svg`, is the old glyphs scaled 1024/824 about the centre, so they keep their size relative to the shape. The system masks the full canvas to its squircle and adds the glass, specular and dark/tinted variants; we do not draw a shape. It can be opened and edited in Xcode's Icon Composer, and the JSON was written by hand, so its defaults (glass on, no explicit shadow or translucency) are the system's.
- **Built at bundle time.** `bundle-macos.sh` runs `xcrun actool assets/logo/fleetd.icon --compile … --platform macosx --minimum-deployment-target 13.0 --app-icon fleetd` and copies the resulting `Assets.car` to `Contents/Resources`. It needs Xcode 26 or later (the GUI already needs Xcode for the Metal toolchain). If actool fails, the script warns and bundles without `Assets.car`; the app then shows the `.icns` in a container on macOS 26. `Assets.car` (about 1.9 MB) is not checked in.
- **`Info.plist`** keeps `CFBundleIconFile = fleetd` and gains `CFBundleIconName = fleetd`. macOS 26 uses the named catalog icon; macOS 13–15 should fall back to the `.icns`, since the catalog holds only the macOS 26 icon (Xcode ships `.icon` apps this way; not tested here, there is no older Mac).
- **macOS 13–15: `assets/logo/fleetd.icns`**, still generated from `fleetd-icon.svg` (inset art on the Big Sur grid, transparent margin, which is right for those systems) and checked in, regenerated with `scripts/bundle-macos.sh --icon-only`. It is rasterised by `scripts/render-svg.swift` (AppKit's SVG support into an RGBA bitmap, alpha kept) instead of `qlmanage`, then `sips` and `iconutil` as before. actool also writes a fallback `.icns` but only up to 256 px, so ours is used instead.

## Consequences

- Two sources for the art: `fleetd-icon.svg` (fallback) and `fleetd.icon/Assets/glyph.svg` plus the fill in `icon.json` (macOS 26). A change to the logo must be made in both.
- Checked by rendering, not by eye: the regenerated `.icns` has transparent corners and no white pixels at any size; the `.icns` actool produces from `fleetd.icon` shows the glyphs on the gradient filling the squircle with transparent corners; the built `fleetd.app` icon read back with `NSWorkspace.shared.icon(forFile:)` is checked the same way. The Dock, ⌘Tab and Launchpad, in light and dark mode, need a look by a person.
- macOS caches icons: a rebuilt app at the same path may show the old icon until `touch fleetd.app` or `killall Dock`.
