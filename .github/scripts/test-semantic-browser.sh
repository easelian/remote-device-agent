#!/usr/bin/env bash
set -euo pipefail

# Only synthetic profiles under the runner's temporary directory are used.
fixture_root="${RUNNER_TEMP:?}/rdh-semantic"
mkdir -p "$fixture_root/evidence" "$fixture_root/firefox-dist"
{
  uname -a
  cat /etc/os-release
  unshare --user --map-root-user --net --pid --mount --fork true
} 2>&1 | tee "$fixture_root/namespace-prerequisite.log"
archive="$fixture_root/firefox-160.0.tar.xz"
curl --fail --location --retry 2 --connect-timeout 15 --max-time 180 \
  https://archive.mozilla.org/pub/firefox/releases/160.0/linux-x86_64/en-US/firefox-160.0.tar.xz \
  --output "$archive"
printf '%s  %s\n' \
  e9c7ed0d4abc6eb3648c2c5652bd325e690423ef49414ef201eac59c0932ea8ec52260cf652510b2c98971c7e7eb36e6be8101d87cae17c6dd28ed763bac32a6 \
  "$archive" | sha512sum --check -
tar -xJf "$archive" -C "$fixture_root/firefox-dist"
export RDH_FIREFOX_EXECUTABLE="$fixture_root/firefox-dist/firefox/firefox"
export RDH_D3_EVIDENCE="$fixture_root/evidence"
export RDH_BIDI_HELPER="$PWD/target/${RDH_TARGET:?}/ci/rdh"
"$RDH_FIREFOX_EXECUTABLE" --version
"${RDH_BROWSER_EXECUTABLE:?}" --version
cargo +1.93.0 build --profile ci --locked -p rdh-agent --all-features --target "$RDH_TARGET" --bin rdh
cargo +1.93.0 test --profile ci --locked -p rdh-agent --all-features --target "$RDH_TARGET" \
  --lib browser::semantic::tests:: -- --ignored --skip resource_cycles --skip performance_budget \
  --nocapture --test-threads=1 2>&1 | tee "$fixture_root/native.log"
