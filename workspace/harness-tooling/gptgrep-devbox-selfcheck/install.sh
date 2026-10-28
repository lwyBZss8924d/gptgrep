#!/bin/sh
# Copy the selected source closure only. No global links, services or runtime probes.
set -eu
if [ "$#" -ne 1 ] || [ "$1" = "--help" ]; then
  echo "Usage: install.sh /absolute/payload-destination"
  [ "$#" -eq 1 ] && [ "$1" = "--help" ] && exit 0
  exit 2
fi
case "$1" in /*) ;; *) echo "Destination must be absolute" >&2; exit 2 ;; esac
OWNER_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DESTINATION=$1
for file in bin/gptgrep-devbox-selfcheck src/selfcheck.ts README.md config.example.json; do
  [ -f "$OWNER_DIR/$file" ] || { echo "Missing canonical selfcheck payload: $file" >&2; exit 1; }
done
mkdir -p "$DESTINATION/bin" "$DESTINATION/src"
for file in bin/gptgrep-devbox-selfcheck src/selfcheck.ts README.md config.example.json; do
  cp "$OWNER_DIR/$file" "$DESTINATION/$file"
done
chmod 0755 "$DESTINATION/bin/gptgrep-devbox-selfcheck"
chmod 0644 "$DESTINATION/src/selfcheck.ts" "$DESTINATION/README.md" "$DESTINATION/config.example.json"
