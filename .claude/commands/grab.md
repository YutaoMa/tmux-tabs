Capture content from a sibling tmux pane and use it as context.

Run the capture command, forwarding any arguments the user provided after `/grab`:

```
tmux-tabs capture $ARGUMENTS
```

## Handling results

**Exit code 0 (success):**
The stdout contains the captured pane content. Present it to the user and use it as context for the current conversation.

**Exit code 1 (error):**
Something went wrong (not in tmux, no panes found). Show the stderr to the user.

**Exit code 2 (ambiguous — multiple candidate panes):**
The stdout contains a JSON object describing the candidate panes. Each pane has:
- `id`: tmux pane ID (e.g. `%7`)
- `command`: the program running in the pane (e.g. `vim`, `cargo`, `python3`)
- `window`: the tmux window name
- `geometry`: `{left, top, width, height}` in character cells — use these to describe the pane's position relative to the current pane (the `caller` field has the caller's pane ID, and the `window` field has the caller's window)
- `activity`: Unix timestamp of last activity

Present a numbered list describing each candidate. For each pane, include:
1. The running command
2. Its spatial position (derive "left", "right", "above", "below" from geometry)
3. The window name (if different from the caller's window)

Ask the user to pick one, then run:
```
tmux-tabs capture --pane <selected_pane_id>
```

Present the captured content and use it as context.
