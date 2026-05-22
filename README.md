# tmux-tabs

Sidebar for managing tmux sessions — with first-class Claude Code integration, plus git and Chrome.

## Requirements

- tmux 3.0+
- Rust toolchain (`cargo`)
- Optional: [`gh`](https://cli.github.com/) CLI (for PR status badges in the sidebar)

## Install

```sh
cargo build --release
cp target/release/tmux-tabs target/release/tmux-tabs-server ~/.cargo/bin/
```

(Or any directory on your `$PATH` — `/usr/local/bin` works too.)

## tmux integration

Add to your tmux config (`~/.tmux.conf` or `~/.config/tmux/tmux.conf`), replacing `/path/to/tmux-tabs` with the path where you cloned this repo:

```tmux
# Auto-create the sidebar pane on new sessions/windows.
set-hook -g session-created 'run-shell "/path/to/tmux-tabs/scripts/tmux-tabs-sidebar.sh"'
set-hook -g after-new-window 'run-shell "/path/to/tmux-tabs/scripts/tmux-tabs-sidebar.sh"'

# Manual toggle: prefix + T
bind-key T run-shell "/path/to/tmux-tabs/scripts/tmux-tabs-sidebar.sh"

# Optional: prefix + g enters a "tabs mode" key table for quick actions.
bind-key g switch-client -T tabs_mode
bind-key -T tabs_mode 1 run-shell "tmux-tabs switch 1"
bind-key -T tabs_mode 2 run-shell "tmux-tabs switch 2"
bind-key -T tabs_mode 3 run-shell "tmux-tabs switch 3"
bind-key -T tabs_mode 4 run-shell "tmux-tabs switch 4"
bind-key -T tabs_mode 5 run-shell "tmux-tabs switch 5"
bind-key -T tabs_mode 6 run-shell "tmux-tabs switch 6"
bind-key -T tabs_mode 7 run-shell "tmux-tabs switch 7"
bind-key -T tabs_mode 8 run-shell "tmux-tabs switch 8"
bind-key -T tabs_mode 9 run-shell "tmux-tabs switch 9"
bind-key -T tabs_mode x display-popup -E "tmux-tabs close"
```

Reload tmux (`prefix + :source-file ~/.tmux.conf`) and start a new session — the sidebar should appear on the left.

The `tmux-tabs-server` daemon starts on demand the first time the sidebar opens.

## Usage

Mouse (works regardless of which pane has focus):

- **Click** a card → switch to that session
- **Scroll wheel** → switch to the previous/next session

From any pane (key tables):

- **`prefix + g + 1..9`** → jump to the Nth session
- **`prefix + g + x`** → close the current session (popup confirms)

Inside the sidebar pane (when focused):

- **`j` / `k`** or **`↓` / `↑`** → move selection
- **`Enter`** → switch to selected session
- **`r`** → rename current/selected session
- **`Esc`** → clear selection
- **`q`** → close the sidebar

## License

MIT — see [LICENSE](LICENSE).
