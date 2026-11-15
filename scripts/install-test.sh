#!/bin/sh
# Runs scripts/install.sh against temp dirs (never the real /Applications,
# ~/.local/bin or ~/.fleet): a fresh install, an install over a running
# daemon with an agent, and a non-fleet file at the link path.
# Not wired into CI; run it by hand: sh scripts/install-test.sh
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)

tmp=$(mktemp -d)
daemon_pid=
cleanup() {
  [ -z "$daemon_pid" ] || kill "$daemon_pid" 2>/dev/null || true
  # A daemon restarted by the script is the same pid; this catches strays.
  pkill -f "fleetd --state-dir $tmp/state" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
install() { sh "$root/scripts/install.sh" --app-dir "$tmp/apps" --bin-dir "$tmp/bin" --state-dir "$tmp/state" "$@"; }

mkdir -p "$tmp/state"

echo "== fresh install, no daemon"
out=$(install 2>&1) || { echo "$out"; fail "install.sh failed"; }
echo "$out"
if [ "$(uname -s)" = Darwin ]; then
  [ -d "$tmp/apps/fleetd.app" ] || fail "no app at $tmp/apps/fleetd.app"
  [ -L "$tmp/bin/fleetd" ] && [ -x "$tmp/bin/fleetd" ] || fail "fleetd link does not resolve"
  [ -L "$tmp/bin/fleet" ] && [ -x "$tmp/bin/fleet" ] || fail "fleet link does not resolve"
else
  [ -x "$tmp/bin/fleetd" ] && [ -x "$tmp/bin/fleet" ] || fail "binaries missing in $tmp/bin"
fi
"$tmp/bin/fleet" --help >/dev/null || fail "fleet --help does not run"
case "$out" in *"no fleetd running"*) ;; *) fail "output lacks 'no fleetd running'" ;; esac

echo "== install over a running daemon"
"$tmp/bin/fleetd" --state-dir "$tmp/state" >/dev/null 2>&1 &
daemon_pid=$!
i=0
until [ -S "$tmp/state/fleetd.sock" ]; do
  i=$((i + 1)); [ $i -lt 100 ] || fail "test daemon did not start"; sleep 0.1
done
"$tmp/bin/fleet" --state-dir "$tmp/state" spawn sh >/dev/null
out=$(install 2>&1) || { echo "$out"; fail "second install.sh failed"; }
echo "$out"
case "$out" in *"fleetd restarted: 1 agents kept"*) ;; *) fail "output lacks 'fleetd restarted: 1 agents kept'" ;; esac
"$tmp/bin/fleet" --state-dir "$tmp/state" ls | grep -q running || fail "agent is not running after restart"

echo "== non-fleet file at the link path"
rm -f "$tmp/bin/fleet"
printf '#!/bin/sh\necho not fleet\n' > "$tmp/bin/fleet"
chmod +x "$tmp/bin/fleet"
if out=$(install 2>&1); then echo "$out"; fail "install.sh replaced an unrelated file"; fi
echo "$out"
case "$out" in *"$tmp/bin/fleet"*) ;; *) fail "output does not name $tmp/bin/fleet" ;; esac
grep -q 'not fleet' "$tmp/bin/fleet" || fail "unrelated file was modified"

echo "PASS"
