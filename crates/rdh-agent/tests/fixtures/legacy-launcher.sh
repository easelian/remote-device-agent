#!/bin/sh
# Run the bundled Agent with the bundled runtime, independent of PATH.
set -eu
target=$0
while [ -L "$target" ]; do
  link=$(readlink "$target")
  case $link in
    /*) target=$link ;;
    *) target=${target%/*}/$link ;;
  esac
done
dir=$(CDPATH= cd -- "${target%/*}" && pwd -P)
exec "$dir/node" "$dir/bin/remote-device-hub.mjs" "$@"
