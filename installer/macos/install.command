#!/bin/bash
set -euo pipefail

APP_DIR="$HOME/Library/Application Support/valorant-watcher"
BIN="$APP_DIR/valorant-watcher"
PLIST="$HOME/Library/LaunchAgents/com.ryleqthereal.valorantwatcher.plist"
SRC="$(cd "$(dirname "$0")" && pwd)"

mkdir -p "$APP_DIR" "$HOME/Library/LaunchAgents"

cp "$SRC/valorant-watcher" "$BIN"
chmod +x "$BIN"
xattr -dr com.apple.quarantine "$BIN" 2>/dev/null || true

# keep an existing config so user edits survive reinstalls
[ -f "$APP_DIR/config.json" ] || cp "$SRC/config.json" "$APP_DIR/config.json"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.ryleqthereal.valorantwatcher</string>
    <key>ProgramArguments</key><array><string>$BIN</string></array>
    <key>WorkingDirectory</key><string>$APP_DIR</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
</dict>
</plist>
EOF

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "valorant-watcher installed and started"
