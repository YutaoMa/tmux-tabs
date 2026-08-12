#!/usr/bin/env bash
set -euo pipefail

# Regenerate the README screenshots in docs/images/.
# Usage: ./scripts/gen-screenshots.sh
#
# The sidebar is drawn by the real UI code (crates/client/src/screenshot.rs)
# into an off-screen buffer and serialised to SVG; headless Chrome then
# rasterises each SVG to a 2x PNG. No tmux session or running server is
# involved, so the output is byte-for-byte reproducible.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/docs/images"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

find_chrome() {
    local candidates=(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        "/Applications/Chromium.app/Contents/MacOS/Chromium"
        "$(command -v google-chrome || true)"
        "$(command -v chromium || true)"
        "$(command -v chromium-browser || true)"
    )
    local c
    for c in "${candidates[@]}"; do
        if [[ -n "$c" && -x "$c" ]]; then
            printf '%s' "$c"
            return 0
        fi
    done
    return 1
}

CHROME="$(find_chrome)" || {
    echo "error: needs Chrome or Chromium to rasterise the screenshots" >&2
    exit 1
}

# shot <name> <width> <height> <url>
# Chrome is noisy on stderr, so it is silenced and the PNG checked instead.
shot() {
    local name=$1 width=$2 height=$3 url=$4
    local png="$OUT/$name.png"
    rm -f "$png"
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --force-device-scale-factor=2 --default-background-color=00000000 \
        --window-size="$width,$height" --screenshot="$png" "$url" 2>/dev/null
    if [[ ! -s "$png" ]]; then
        echo "error: Chrome produced no output for $name" >&2
        exit 1
    fi
    echo "wrote $png (${width}x${height} @2x)"
}

mkdir -p "$OUT"
cargo run --quiet --manifest-path "$ROOT/Cargo.toml" -p tmux-tabs-client \
    --features screenshots -- __screenshot "$WORK"

for svg in "$WORK"/*.svg; do
    # The SVG root carries the exact pixel size; match the viewport to it so
    # nothing is cropped or letterboxed.
    read -r w h < <(sed -n 's/.*width="\([0-9]*\)" height="\([0-9]*\)".*/\1 \2/p' "$svg" | head -1)
    shot "$(basename "$svg" .svg)" "$w" "$h" "file://$svg"
done

# The extension popup is rendered from the real popup.html/popup.js, with only
# the chrome.* data source stubbed out (headless Chrome has no tab groups).
# The window size is the popup's natural size (240px body + 12px padding, one
# row per group); bump the height if popup.html grows.
POPUP="$WORK/popup"
mkdir -p "$POPUP"
cp "$ROOT/extension/popup.js" "$POPUP/"
cat > "$POPUP/demo-data.js" <<'EOF'
const GROUPS = [
  { id: 1, title: "api-gateway", tabs: 4 },
  { id: 2, title: "tmux-tabs", tabs: 2 },
  { id: 3, title: "tonic", tabs: 3 },
];
window.chrome = {
  tabGroups: { query: async () => GROUPS },
  tabs: {
    query: async ({ groupId }) =>
      new Array(GROUPS.find((g) => g.id === groupId).tabs).fill({}),
  },
};
EOF
sed 's|<script src="popup.js">|<script src="demo-data.js"></script><script src="popup.js">|' \
    "$ROOT/extension/popup.html" > "$POPUP/popup.html"
shot chrome-popup 264 152 "file://$POPUP/popup.html"

# Chrome ignores --load-extension on current builds, so the tab strip and the
# right-click menu cannot be captured from a real browser. Those two are
# hand-built HTML illustrations; each file's header records what it keeps
# faithful to the extension.
shot chrome-tab-groups 880 98 "file://$ROOT/scripts/mockups/chrome-tab-groups.html"
shot chrome-send-to-ai 700 420 "file://$ROOT/scripts/mockups/chrome-send-to-ai.html"
