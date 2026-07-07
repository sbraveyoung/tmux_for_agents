#!/usr/bin/env bash
# Thin forwarder: locate tfa and hand off. Any failure must be silent —
# this runs inside agent hook paths and must never block the agent.
if [ -n "$TFA_BIN" ] && [ -x "$TFA_BIN" ]; then
  BIN="$TFA_BIN"
elif command -v tfa >/dev/null 2>&1; then
  BIN="tfa"
elif [ -x "$HOME/.cargo/bin/tfa" ]; then
  BIN="$HOME/.cargo/bin/tfa"
else
  exit 0
fi
exec "$BIN" hook "$@"
