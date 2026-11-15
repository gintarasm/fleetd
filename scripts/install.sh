#!/bin/sh
# Builds fleetd, installs it and moves the running daemon onto the new build:
#   git pull && scripts/install.sh
#
#   --app-dir <dir>    macOS: where fleetd.app goes (default /Applications)
#   --bin-dir <dir>    where `fleetd` and `fleet` go (default ~/.local/bin); on
#                      macOS they are links into the app, on Linux the binaries
#   --state-dir <dir>  state directory of the daemon to restart (default ~/.fleet)
#   --force-stop       if the running daemon is too old to restart itself, stop
#                      it (its agents end) instead of leaving it for you to stop
#
# Every step prints one line saying what it did.
set -eu
cd "$(dirname "$0")/.."
repo=$(pwd)

app_dir=/Applications
bin_dir=$HOME/.local/bin
state_dir=$HOME/.fleet
force_stop=0
while [ $# -gt 0 ]; do
  case "$1" in
    --app-dir | --bin-dir | --state-dir)
      [ $# -ge 2 ] || { echo "install: $1 needs a directory" >&2; exit 64; }
      case "$1" in
        --app-dir) app_dir=$2 ;;
        --bin-dir) bin_dir=$2 ;;
        --state-dir) state_dir=$2 ;;
      esac
      shift 2
      ;;
    --force-stop) force_stop=1; shift ;;
    -h | --help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install: unknown option $1 (see --help)" >&2; exit 64 ;;
  esac
done

die() { echo "install: $*" >&2; exit 1; }

# Resolves a directory to an absolute path, creating it if needed, so links
# and the daemon's exec target stay valid whatever the current directory is.
abs_dir() { mkdir -p "$1" && (cd "$1" && pwd); }

# What counts as "ours" when something already sits at a path we want to
# replace. Neither binary has `--version`: fleetd answers `--handoff-check`
# (`handoff <n>`; builds before the restart feature reject it with a usage
# line), fleet's `--help` carries its tagline.
is_fleetd() { "$1" --handoff-check 2>&1 | grep -Eq '^handoff [0-9]|^usage: fleetd'; }
is_fleet() { "$1" --help 2>&1 | grep -q 'Manage agents run by fleetd'; }

# Refuses to overwrite an unrelated regular file at `$1` (`$2` is fleetd or
# fleet); a missing path or a symlink (ours from last time, or cargo's) is fine.
check_replaceable() {
  [ -e "$1" ] || [ -L "$1" ] || return 0
  [ -L "$1" ] && return 0
  [ -f "$1" ] && "is_$2" "$1" && return 0
  die "$1 is not a $2 binary; not replacing it. Move it away and run again."
}

app_dir=$(abs_dir "$app_dir")
bin_dir=$(abs_dir "$bin_dir")
os=$(uname -s)

# Fail before the long build, not after it.
check_replaceable "$bin_dir/fleetd" fleetd
check_replaceable "$bin_dir/fleet" fleet

# Replaces `$2` with a copy of `$1` without ever leaving it half-written:
# copy beside it under a temporary name, then rename.
install_file() {
  tmp="$2.new.$$"
  cp "$1" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$2" || { rm -f "$tmp"; die "could not install $2"; }
}

if [ "$os" = Darwin ]; then
  echo "building fleetd.app (a few minutes the first time)..."
  sh scripts/bundle-macos.sh
  new="$app_dir/.fleetd.app.new.$$"
  old="$app_dir/.fleetd.app.old.$$"
  rm -rf "$new"
  cp -R target/release/fleetd.app "$new" || { rm -rf "$new"; die "could not copy the app into $app_dir"; }
  # A directory cannot be renamed over a non-empty one, so the old copy steps
  # aside first; it comes back if the new one cannot take its place.
  if [ -e "$app_dir/fleetd.app" ]; then
    mv "$app_dir/fleetd.app" "$old" || { rm -rf "$new"; die "could not move the old $app_dir/fleetd.app aside"; }
  fi
  if ! mv "$new" "$app_dir/fleetd.app"; then
    [ ! -e "$old" ] || mv "$old" "$app_dir/fleetd.app"
    rm -rf "$new"
    die "could not install $app_dir/fleetd.app"
  fi
  rm -rf "$old"
  echo "installed $app_dir/fleetd.app"
  for b in fleetd fleet; do
    target="$app_dir/fleetd.app/Contents/MacOS/$b"
    ln -s "$target" "$bin_dir/$b.new.$$" && mv -f "$bin_dir/$b.new.$$" "$bin_dir/$b" || die "could not link $bin_dir/$b"
    echo "linked $bin_dir/$b -> $target"
  done
else
  echo "building fleetd and fleet..."
  cargo build --release -p fleetd -p fleet-cli
  # cargo honours CARGO_TARGET_DIR; copying from ./target would pick up a stale
  # (or foreign) build.
  for b in fleetd fleet; do
    install_file "${CARGO_TARGET_DIR:-target}/release/$b" "$bin_dir/$b"
    echo "installed $bin_dir/$b"
  done
fi

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "warning: $bin_dir is not on your PATH; add: export PATH=\"$bin_dir:\$PATH\"" ;;
esac

fleet="$bin_dir/fleet"
sock="$state_dir/fleetd.sock"
if ! "$fleet" --state-dir "$state_dir" ls >/dev/null 2>&1; then
  echo "no fleetd running; the next client starts the new one"
  exit 0
fi

echo "restarting fleetd onto the new build..."
rc=0
out=$("$fleet" --state-dir "$state_dir" daemon restart --binary "$bin_dir/fleetd" 2>&1) || rc=$?
if [ "$rc" -eq 0 ]; then
  echo "$out"
  exit 0
fi
echo "$out" >&2
case "$out" in
  *"too old to restart itself"*) ;;
  *) exit "$rc" ;;
esac

if [ "$force_stop" -eq 0 ]; then
  echo "install: to stop it now, run again with --force-stop (its running agents will end)" >&2
  exit 1
fi
echo "stopping fleetd: its running agents will end"
lock="$state_dir/fleetd.lock"
if command -v lsof >/dev/null 2>&1; then
  pids=$(lsof -t "$lock" 2>/dev/null || true)
elif command -v fuser >/dev/null 2>&1; then
  pids=$(fuser "$lock" 2>/dev/null || true)
else
  die "cannot find which process runs fleetd: neither lsof nor fuser is installed; stop fleetd by hand (pkill fleetd)"
fi
pids=$(echo $pids)
[ -n "$pids" ] || die "no process holds $lock; stop fleetd by hand"
case "$pids" in
  *" "*) die "more than one process holds $lock ($pids); stop fleetd by hand" ;;
esac
kill -TERM "$pids"
i=0
while [ -e "$sock" ]; do
  i=$((i + 1))
  [ "$i" -le 50 ] || die "fleetd (pid $pids) did not stop within 5 s"
  sleep 0.1
done
echo "fleetd stopped; the next client starts the new one"
