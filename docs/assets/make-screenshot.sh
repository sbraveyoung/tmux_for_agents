#!/bin/sh
# Regenerate docs/assets/tfa-tui.png: fabricated demo snapshot -> isolated
# daemon + tmux -> capture-pane ANSI -> aha (HTML) -> headless Chrome (PNG).
# Deps: aha (brew install aha), Google Chrome. Run from the repo root.
set -e
REPO=$(cd "$(dirname "$0")/../.." && pwd)
ASSETS="$REPO/docs/assets"
DEMO=$(mktemp -d)
SOCK=tfashot
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

python3 "$ASSETS/demo-snapshot.py" "$DEMO/snapshot.json"
printf '[tui]\nlang = "en"\ncolor = true\n' > "$DEMO/config.toml"
ENVS="TFA_SOCKET=$DEMO/tfa.sock TFA_STATE_DIR=$DEMO TFA_CONFIG_PATH=$DEMO/config.toml \
TFA_NO_SCAN=1 TFA_SKIP_TMUX_CHECK=1 TFA_NO_NOTIFY=1 TFA_NO_SPAWN=1"

cargo build --quiet
env $ENVS "$REPO/target/debug/tfa" daemon >/dev/null 2>&1 & DPID=$!
trap 'kill $DPID 2>/dev/null; tmux -f /dev/null -L $SOCK kill-server 2>/dev/null' EXIT
sleep 1
[ -S "$DEMO/tfa.sock" ] || { echo "ERROR: daemon failed to start"; exit 1; }

tmux -f /dev/null -L $SOCK kill-server 2>/dev/null || true
tmux -f /dev/null -L $SOCK new-session -d -x 120 -y 24 "env $ENVS $REPO/target/debug/tfa tui"
sleep 2
tmux -f /dev/null -L $SOCK send-keys -t 0 Down
sleep 1
tmux -f /dev/null -L $SOCK capture-pane -e -p -t 0 > "$DEMO/tui.ansi"
grep -q "Sessions" "$DEMO/tui.ansi" || { echo "ERROR: capture doesn't look like the TUI"; exit 1; }

aha --black < "$DEMO/tui.ansi" > "$DEMO/tui-raw.html"
python3 - "$DEMO/tui-raw.html" "$DEMO/tui.html" <<'PYEOF'
import sys
raw = open(sys.argv[1]).read()
style = """<style>
body { background:#16181d; margin:0; padding:28px; }
pre { font-family: "SF Mono", Menlo, monospace; font-size:15px; line-height:1.32;
      color:#e6edf3; margin:0; }
</style>"""
open(sys.argv[2], "w").write(raw.replace("</head>", style + "</head>"))
PYEOF
"$CHROME" --headless --disable-gpu --screenshot="$ASSETS/tfa-tui.png" \
  --window-size=1180,560 --hide-scrollbars "file://$DEMO/tui.html" 2>/dev/null
echo "wrote $ASSETS/tfa-tui.png"
