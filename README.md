# tfa — tmux for agents

AI coding agent observability for tmux: who's working, who's waiting
for you, who's done — in your status bar.

## Install (M1)

    cargo install --path .

Claude Code integration (inside claude):

    /plugin marketplace add ~/code/src/github.com/sbraveyoung/tmux_for_agents
    /plugin install tfa@tfa

tmux status bar (~/.tmux.conf):

    set -g status-interval 5
    set -g status-right '#(tfa status --format tmux) | %H:%M'

New claude sessions appear automatically. Existing sessions appear
after their next prompt, or restart them with `claude -c`.

## Environment variables

- `TFA_BIN` — absolute path to the `tfa` binary, used by `hook.sh` to
  locate it. Set this if `~/.cargo/bin` isn't on the `PATH` a
  GUI-spawned Claude Code process sees (a common source of "hooks
  silently do nothing").
- `TFA_SOCKET` — override the daemon's Unix socket path (default
  `/tmp/tfa-<uid>/tfa.sock`).
- `TFA_STATE_DIR` — override the daemon's state directory, where the
  snapshot, lock file, and activity-throttle markers live (default
  `~/.local/state/tfa`).

Testing/advanced (not needed for normal use):

- `TFA_TMUX_SOCKET` — testing/advanced: point at an isolated tmux
  server (`tmux -L <name>`) instead of the default.
- `TFA_NO_SPAWN` — testing/advanced: disable autospawning the daemon
  when a connection fails.
- `TFA_SKIP_TMUX_CHECK` — testing/advanced: skip the daemon's
  liveness check against a real tmux server.
- `TFA_TMUX_CHECK_INTERVAL_MS` — testing/advanced: override the
  daemon's tmux-liveness poll interval (default 10000).
