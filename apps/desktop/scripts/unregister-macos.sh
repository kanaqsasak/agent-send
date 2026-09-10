#!/bin/sh
set -eu
PLIST="$HOME/Library/LaunchAgents/com.agent-send.desktop.plist"
launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
rm -f "$PLIST"
