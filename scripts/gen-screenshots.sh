#!/usr/bin/env bash
set -euo pipefail

# Regenerate the README screenshots in docs/images/.
# Usage: ./scripts/gen-screenshots.sh
#
# The sidebar is drawn by the real UI code (crates/client/src/screenshot.rs)
# into an off-screen buffer and serialised to SVG; headless Chrome then
# rasterises each SVG to a 2x PNG, and animations are stitched into a looping
# APNG by crates/client/src/apng.rs. No tmux session or running server is
# involved, so the output is byte-for-byte reproducible.
#
# Needs Chrome or Chromium.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/docs/images"
WORK="$(mktemp -d)"
SCALE=2
trap 'rm -rf "$WORK"' EXIT

# Both dev-only subcommands live behind the `screenshots` feature.
generator() {
    cargo run --quiet --manifest-path "$ROOT/Cargo.toml" -p tmux-tabs-client \
        --features screenshots -- "$@"
}

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

# render <png> <width> <height> <url>
# Chrome is noisy on stderr, so it is silenced and the PNG checked instead.
render() {
    local png=$1 width=$2 height=$3 url=$4
    rm -f "$png"
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --force-device-scale-factor="$SCALE" --default-background-color=00000000 \
        --window-size="$width,$height" --screenshot="$png" "$url" 2>/dev/null
    if [[ ! -s "$png" ]]; then
        echo "error: Chrome produced no output for $png" >&2
        exit 1
    fi
}

# shot <name> <width> <height> <url> — a still, straight into docs/images.
shot() {
    local name=$1 width=$2 height=$3 url=$4
    render "$OUT/$name.png" "$width" "$height" "$url"
    echo "wrote $OUT/$name.png (${width}x${height} @${SCALE}x)"
}

# The SVG root carries the exact pixel size; match the viewport to it so
# nothing is cropped or letterboxed.
svg_size() {
    sed -n 's/.*width="\([0-9]*\)" height="\([0-9]*\)".*/\1 \2/p' "$1" | head -1
}

# html_anim <name> <width> <height> <file> <fragment:delay_ms>...
# Renders one whole-canvas frame per stage of a mock-up, which selects its
# stage from the URL fragment. The generator finds the changed region itself,
# since there are no terminal cells to diff here.
html_anim() {
    local name=$1 width=$2 height=$3 file=$4
    shift 4
    local dir="$WORK/$name"
    mkdir -p "$dir"
    : > "$dir/frames.txt"
    local index=0 spec idx
    for spec in "$@"; do
        printf -v idx '%03d' "$index"
        render "$dir/$idx.png" "$width" "$height" "file://$file#${spec%%:*}"
        printf '%s %s\n' "$idx" "${spec##*:}" >> "$dir/frames.txt"
        index=$((index + 1))
    done
    generator __apng "$OUT/$name.png" "$dir" "$SCALE"
}

mkdir -p "$OUT"
generator __screenshot "$WORK"

for svg in "$WORK"/*.svg; do
    read -r w h < <(svg_size "$svg")
    shot "$(basename "$svg" .svg)" "$w" "$h" "file://$svg"
done

# Animations: each subdirectory holds one SVG per frame plus a manifest giving
# each frame's offset into the canvas. Every frame after the first covers only
# the region that changed, so they rasterise fast and stitch into a small APNG.
for dir in "$WORK"/*/; do
    [[ -f "$dir/000.svg" ]] || continue
    name="$(basename "$dir")"
    while read -r idx _; do
        read -r w h < <(svg_size "$dir/$idx.svg")
        render "$dir/$idx.png" "$w" "$h" "file://$dir/$idx.svg"
    done < "$dir/frames.txt"
    generator __apng "$OUT/$name.png" "$dir" "$SCALE"
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
# faithful to the extension. Both are animated by stepping the stage named in
# the URL fragment.
html_anim chrome-tab-groups 880 98 "$ROOT/scripts/mockups/chrome-tab-groups.html" \
    tmux-tabs:1500 api-gateway:1700 tonic:2600

html_anim chrome-send-to-ai 700 540 "$ROOT/scripts/mockups/chrome-send-to-ai.html" \
    0:900 1:750 2:700 3:950 4:2600
