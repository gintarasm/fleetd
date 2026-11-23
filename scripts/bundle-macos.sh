#!/bin/sh
# Builds <target>/release/fleetd.app (fleet-gui + fleetd + fleet), where
# <target> is CARGO_TARGET_DIR (relative: from the repo root), else target.
# --icon-only regenerates assets/logo/fleetd.icns from assets/logo/fleetd-icon.svg.
# The app icon (ADR 0104): on macOS 26 the system uses Assets.car, compiled here from the
# Icon Composer bundle assets/logo/fleetd.icon with actool (Xcode 26+), named by
# CFBundleIconName; older macOS uses fleetd.icns (CFBundleIconFile).
# Building fleet-gui needs the Metal toolchain: xcodebuild -downloadComponent MetalToolchain
set -eu
cd "$(dirname "$0")/.."

make_icon() {
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  # Not qlmanage: it fills the transparent margin white.
  src="$tmp/fleetd-icon.png"
  swift scripts/render-svg.swift assets/logo/fleetd-icon.svg "$src" 1024
  set_dir="$tmp/fleetd.iconset"
  mkdir "$set_dir"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$src" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$src" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$set_dir" -o assets/logo/fleetd.icns
}

if [ "${1:-}" = "--icon-only" ]; then
  make_icon
  echo "wrote assets/logo/fleetd.icns"
  exit 0
fi

# Where cargo builds (it runs here, in the repo root): copying from ./target
# instead would bundle a stale build.
target_dir=${CARGO_TARGET_DIR:-target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$(pwd)/$target_dir" ;;
esac
cargo build --release -p fleet-gui -p fleetd -p fleet-cli
release="$target_dir/release"
app="$release/fleetd.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$release/fleet-gui" "$release/fleetd" "$release/fleet" "$app/Contents/MacOS/"
[ -f assets/logo/fleetd.icns ] || make_icon
cp assets/logo/fleetd.icns "$app/Contents/Resources/fleetd.icns"
# actool also writes a fallback fleetd.icns (at most 256 px); ours is kept instead.
car_tmp=$(mktemp -d)
if xcrun actool assets/logo/fleetd.icon --compile "$car_tmp" --platform macosx \
  --minimum-deployment-target 13.0 --app-icon fleetd \
  --output-partial-info-plist "$car_tmp/partial.plist" >/dev/null 2>&1 &&
  [ -f "$car_tmp/Assets.car" ]; then
  cp "$car_tmp/Assets.car" "$app/Contents/Resources/Assets.car"
else
  echo "warning: actool (Xcode 26+) could not compile assets/logo/fleetd.icon;" \
    "macOS 26 will show fleetd.icns in a container" >&2
fi
rm -rf "$car_tmp"
version=$(grep -m1 '^version' crates/fleet-gui/Cargo.toml | cut -d'"' -f2)
sed "s/__VERSION__/${version:-0.1.0}/g" crates/fleet-gui/Info.plist > "$app/Contents/Info.plist"
echo "built $app"
