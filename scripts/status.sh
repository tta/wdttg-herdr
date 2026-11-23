#!/bin/sh
# Print the current tracking status for the herdr tab bar.
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-.}"
STATUS_FILE="$STATE_DIR/status.txt"
if [ -f "$STATUS_FILE" ]; then
    cat "$STATUS_FILE"
else
    printf '○ wdttg idle'
fi
exit 0
