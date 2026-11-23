#!/bin/sh
# Resume time tracking.
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-.}"
rm -f "$STATE_DIR/paused"
exit 0
