#!/bin/sh
# Run from the installed app's Resources directory. Preserve user data.
set -eu
resources=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
executable="$resources/../MacOS/lan-mouse"
if [ ! -x "$executable" ]; then
    echo "Run the copy inside Sharemouse.app/Contents/Resources." >&2
    exit 1
fi
# A stopped app returns a nonzero CLI connection result.
"$executable" cli quit || true
"$executable" cli integration uninstall
agent="$HOME/Library/LaunchAgents/de.feschber.LanMouse.plist"
if [ -f "$agent" ]; then
    rm "$agent"
fi
echo "Login and Finder registrations removed. Move Sharemouse.app to Trash."
echo "Received files, clipboard history, configuration and identity were preserved."
