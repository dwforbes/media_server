#!/bin/sh
# Update the macOS announcer LaunchDaemon from the repo build.
# The daemon runs from the INTERNAL disk (system processes cannot read
# external volumes), so each update is: build here, stop, copy, re-sign
# (the ad-hoc signature is per-binary; skipping this breaks the Local
# Network grant binding), restart.
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PLIST=/Library/LaunchDaemons/com.mediaserver.announcer.plist
BIN=/usr/local/bin/media-announcer

cd "$REPO"
git pull
cargo build --release -p media-announcer

sudo launchctl unload "$PLIST"
sudo cp target/release/media-announcer "$BIN"
sudo codesign -s - -i com.mediaserver.announcer --force "$BIN"
sudo launchctl load "$PLIST"

echo "updated; watching log (ctrl-c to stop):"
tail -f /tmp/media-announcer.log
