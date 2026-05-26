#!/usr/bin/env bash
# Claude Code hook script. Forwards an event to the tmux-tabs server.
# Usage: tmux-tabs-hook.sh <event_name>
# Events: session_start, prompt_submit, tool_use, stop, session_end, notification
# Stdin: JSON payload from Claude Code (piped through to tmux-tabs notify).
#
# This script does no tmux lookups itself. The Rust binary reads TMUX_PANE from
# the environment and the tmux-tabs server resolves the session name from its
# cached pane map, which keeps Claude Code's hook latency minimal.

set -euo pipefail

EVENT="${1:-}"
[ -z "${TMUX_PANE:-}" ] && exit 0
[ -z "$EVENT" ] && exit 0

# Capture stdin so this script can return immediately (the Rust call runs in
# the background).
STDIN_DATA=$(cat)

echo "$STDIN_DATA" | tmux-tabs notify "$EVENT" &>/dev/null &
exit 0
