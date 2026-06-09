#!/usr/bin/env bash
# Copilot CLI hook script. Registers a new Copilot session with the
# tmux-tabs server so it can start tailing the session's events.jsonl.
#
# Wire up in ~/.copilot/settings.json:
#   { "version": 1,
#     "hooks": {
#       "sessionStart": [{
#         "type": "command",
#         "command": "bash /path/to/tmux-tabs/scripts/tmux-tabs-copilot-hook.sh",
#         "timeoutSec": 5
#       }]
#     }
#   }
#
# Stdin: JSON payload from Copilot CLI. We only need `sessionId` from it;
# the server discovers everything else by reading the per-session
# events.jsonl file directly.

set -euo pipefail

[ -z "${TMUX_PANE:-}" ] && exit 0

STDIN_DATA=$(cat)

# Prefer jq; fall back to a tolerant grep so the hook still works in a
# stripped environment.
if command -v jq >/dev/null 2>&1; then
  SESSION_ID=$(printf '%s' "$STDIN_DATA" | jq -r '.sessionId // empty' 2>/dev/null || true)
else
  SESSION_ID=$(printf '%s' "$STDIN_DATA" \
    | grep -o '"sessionId"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 \
    | sed -E 's/.*"([^"]*)"$/\1/')
fi

[ -z "${SESSION_ID:-}" ] && exit 0

tmux-tabs notify sessionStart \
  --agent copilot \
  --copilot-session-id "$SESSION_ID" &>/dev/null &

exit 0
