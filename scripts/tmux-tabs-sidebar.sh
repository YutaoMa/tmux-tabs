#!/usr/bin/env bash
# Creates a tmux-tabs sidebar pane in the current window if one doesn't exist.
# Called by tmux hooks (session-created, after-new-window) and from a manual
# key binding.

set -euo pipefail

SIDEBAR_WIDTH=24

EXISTING=$(tmux list-panes -F '#{pane_title}' 2>/dev/null | grep -c '^tmux-tabs$' || true)
if [ "$EXISTING" -gt 0 ]; then
    exit 0
fi

tmux split-window -hbdl "$SIDEBAR_WIDTH" "tmux-tabs"
