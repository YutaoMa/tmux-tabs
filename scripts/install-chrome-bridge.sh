#!/usr/bin/env bash
set -euo pipefail

# Install the tmux-tabs-bridge native messaging host for Chrome.
# Usage: ./scripts/install-chrome-bridge.sh [path-to-bridge-binary]

BINARY="${1:-$(command -v tmux-tabs-bridge 2>/dev/null || echo "")}"

if [[ -z "$BINARY" ]]; then
    # Try cargo build output.
    CARGO_BIN="$(cd "$(dirname "$0")/.." && cargo metadata --format-version 1 2>/dev/null \
        | grep -o '"target_directory":"[^"]*"' | head -1 | cut -d'"' -f4)/debug/tmux-tabs-bridge"
    if [[ -f "$CARGO_BIN" ]]; then
        BINARY="$CARGO_BIN"
    else
        echo "Error: tmux-tabs-bridge binary not found."
        echo "Build it first: cargo build -p tmux-tabs-bridge"
        echo "Or pass the path: $0 /path/to/tmux-tabs-bridge"
        exit 1
    fi
fi

BINARY="$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"

if [[ ! -x "$BINARY" ]]; then
    echo "Error: $BINARY is not executable"
    exit 1
fi

echo "Using bridge binary: $BINARY"

# Determine the native messaging host manifest directory.
case "$(uname -s)" in
    Darwin)
        MANIFEST_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
        ;;
    Linux)
        MANIFEST_DIR="$HOME/.config/google-chrome/NativeMessagingHosts"
        ;;
    *)
        echo "Error: unsupported platform $(uname -s)"
        exit 1
        ;;
esac

mkdir -p "$MANIFEST_DIR"

MANIFEST_PATH="$MANIFEST_DIR/com.tmux_tabs.bridge.json"

cat > "$MANIFEST_PATH" <<EOF
{
  "name": "com.tmux_tabs.bridge",
  "description": "tmux-tabs browser integration bridge",
  "path": "$BINARY",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://EXTENSION_ID_PLACEHOLDER/"
  ]
}
EOF

echo "Installed native messaging host manifest to:"
echo "  $MANIFEST_PATH"
echo ""
echo "NOTE: After loading the extension in Chrome, replace EXTENSION_ID_PLACEHOLDER"
echo "in the manifest with your extension's actual ID."
echo "You can find the ID at chrome://extensions (enable Developer mode)."
