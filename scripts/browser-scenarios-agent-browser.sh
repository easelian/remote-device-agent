#!/bin/bash
# Drive the scenario pages of tests/fixtures/browser-scenarios with agent-browser,
# for comparison with tests/browser_scenarios.rs. Prints one JSON line per scenario:
# calls, bytes returned to the caller, what the site received, and notes.
#
# Usage: scripts/browser-scenarios-agent-browser.sh /path/to/chrome [/path/to/agent-browser]
set -u
chrome=${1:?browser executable}
cli=${2:-$HOME/.local/bin/agent-browser}
root=$(cd "$(dirname "$0")/.." && pwd)
port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
python3 "$root/tests/fixtures/browser-scenarios/server.py" "$port" >/dev/null &
site=$!
trap 'kill $site 2>/dev/null' EXIT
sleep 0.5
tmp=$(mktemp -d)
session="scenarios$$"
calls=0; bytes=0; notes=""
# A browser started from a desktop or ssh environment can hang on navigation; use a bare one.
ab() {
  calls=$((calls+1))
  env -i HOME="$HOME" PATH=/usr/bin:/bin LANG=C.UTF-8 AGENT_BROWSER_NO_VERSION_CHECK=1 \
    "$cli" --session "$session" --executable-path "$chrome" "$@" >"$tmp/out" 2>&1
  local status=$?
  bytes=$((bytes+$(wc -c <"$tmp/out")))
  return $status
}
snap() { ab snapshot -i; cp "$tmp/out" "$tmp/snap"; }
# The ref on the first snapshot line that holds every given text.
ref() { local lines; lines=$(cat "$tmp/snap"); for text in "$@"; do lines=$(grep -F -- "$text" <<<"$lines"); done
  grep -o 'ref=e[0-9]*' <<<"$lines" | head -1 | cut -d= -f2; }
note() { notes="$notes${notes:+; }$1"; }
try() { local label=$1; shift; if ab "$@"; then note "$label: done"; else note "$label: refused"; fi; }
begin() { calls=0; bytes=0; notes=""; ab open "http://127.0.0.1:$port/$1.html"; }
finish() {
  local writes; writes=$(curl -s "http://127.0.0.1:$port/audit" | python3 -c "import json,sys;print(json.dumps(json.load(sys.stdin).get('$1',[])))")
  python3 -c "import json,sys;print(json.dumps({'scenario':sys.argv[1],'calls':int(sys.argv[2]),'bytes':int(sys.argv[3]),'writes':json.loads(sys.argv[4]),'notes':sys.argv[5]}))" "$1" "$calls" "$bytes" "$writes" "$notes"
}

begin recycle; snap; old=$(ref 'button "Approve"' 'Order 0003')
# Scroll as a user would and let the page redraw before going on.
ab eval "const l=document.getElementById('list');l.scrollTop=400;l.dispatchEvent(new Event('scroll'));l.textContent.slice(0,10)"
note "list now starts with $(tr -d '\n"' <"$tmp/out")"
try "ref taken for Order 0003, used after its row shows another order" click "@$old"
finish recycle

begin rerender; snap; second=$(ref 'button "Remove"' 'Item 2'); fifth=$(ref 'button "Remove"' 'Item 5')
ab click "@$second"; snap; fourth=$(ref 'button "Remove"' 'Item 4')
try "fresh ref for Item 4" click "@$fourth"
try "ref for Item 5 from before the rerender" click "@$fifth"
finish rerender

begin css-state; snap; box=$(ref 'Show advanced')
ab check "@$box"; snap
grep -qF 'Reset everything' "$tmp/snap" && note "panel listed after a new snapshot" || note "panel missing"
finish css-state

begin modal; snap; second=$(ref 'button "Delete"' 'B42'); third=$(ref 'button "Delete"' 'B43')
ab click "@$second"; snap
grep -qF 'B42' <<<"$(grep -iF 'delete order' "$tmp/snap")" && note "dialog names B42" || note "dialog text not in interactive snapshot"
ab click "@$(ref 'button "Confirm"')"
try "ref for B43 from the first snapshot" click "@$third"; snap
ab click "@$(ref 'button "Cancel"')"
finish modal

begin optimistic; snap
ab click "@$(ref 'button "Approve"' 'A17')"; ab snapshot
grep -qF 'Approved' "$tmp/out" && note "snapshot right after the click shows Approved"
sleep 0.7; ab snapshot
grep -qF 'Rejected' "$tmp/out" && note "a later snapshot shows the refusal"
finish optimistic

begin spa; snap; home=$(ref 'button "Home"'); orders=$(ref 'button "Orders"')
ab click "@$orders"; snap
ab click "@$(ref 'button "Approve"' 'S03')"
try "Home ref from before the route change" click "@$home"
finish spa

begin ticker; snap; second=$(ref 'button "Approve"' 'T02'); third=$(ref 'button "Approve"' 'T03')
ab click "@$second"; ab snapshot -i --delta
note "delta after the click: $(wc -c <"$tmp/out") bytes"
try "T03 ref from the first snapshot" click "@$third"
finish ticker

begin dependent; snap
ab select "@$(ref 'Country')" jp; snap
ab select "@$(ref 'City')" Osaka
ab click "@$(ref 'button "Save address"')"
finish dependent

ab close
rm -rf "$tmp"
