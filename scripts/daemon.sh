#!/bin/sh
# Start the wdttg-herdr daemon if not already running.
# Invoked by the herdr plugin [[startup]] hook.
set -u

STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-.}"
PLUGIN_ROOT="${HERDR_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
PID_FILE="$STATE_DIR/wdttg-herdr.pid"
LOG_FILE="$STATE_DIR/daemon.log"

mkdir -p "$STATE_DIR"

# Already running?
if [ -f "$PID_FILE" ]; then
    old_pid=$(cat "$PID_FILE" 2>/dev/null)
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
        exit 0
    fi
fi

DAEMON="$PLUGIN_ROOT/target/release/wdttg-herdr"
if [ ! -x "$DAEMON" ]; then
    DAEMON="$PLUGIN_ROOT/target/debug/wdttg-herdr"
fi
if [ ! -x "$DAEMON" ]; then
    echo "wdttg-herdr: daemon binary not found (run 'cargo build --release')" >&2
    exit 1
fi

setsid "$DAEMON" >>"$LOG_FILE" 2>&1 &
echo $! >"$PID_FILE"
exit 0
