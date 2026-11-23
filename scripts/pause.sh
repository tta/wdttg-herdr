#!/bin/sh
# Pause time tracking. The daemon polls this flag file.
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-.}"
mkdir -p "$STATE_DIR"
touch "$STATE_DIR/paused"
exit 0
