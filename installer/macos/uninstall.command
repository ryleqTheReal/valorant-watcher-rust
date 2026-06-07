#!/bin/bash
set -uo pipefail

PLIST="$HOME/Library/LaunchAgents/com.ryleqthereal.valorantwatcher.plist"
APP_DIR="$HOME/Library/Application Support/valorant-watcher"

launchctl unload "$PLIST" 2>/dev/null || true
rm -f "$PLIST"
rm -rf "$APP_DIR"

echo "valorant-watcher uninstalled"
