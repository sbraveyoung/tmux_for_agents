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
  `/tmp/tfa-<uid>/tfa.sock`, but `$XDG_RUNTIME_DIR/tfa/tfa.sock` is used
  instead when `XDG_RUNTIME_DIR` is set — e.g. under systemd user
  sessions on Linux).
- `TFA_STATE_DIR` — override the daemon's state directory, where the
  snapshot, lock file, and activity-throttle markers live (default
  `~/.local/state/tfa`).
- `TFA_SCAN_INTERVAL_MS` — override the background scanner's poll
  interval (default `15000`).
- `TFA_CLAUDE_PROJECTS_DIR` — override where the scanner looks for
  Claude Code session transcripts (default `~/.claude/projects`).
- `TFA_CODEX_DB` — override the path to the Codex sqlite state
  database (default `~/.codex/state_5.sqlite`).

Testing/advanced (not needed for normal use):

- `TFA_TMUX_SOCKET` — testing/advanced: point at an isolated tmux
  server (`tmux -L <name>`) instead of the default.
- `TFA_NO_SPAWN` — testing/advanced: disable autospawning the daemon
  when a connection fails.
- `TFA_SKIP_TMUX_CHECK` — testing/advanced: skip the daemon's
  liveness check against a real tmux server.
- `TFA_TMUX_CHECK_INTERVAL_MS` — testing/advanced: override the
  daemon's tmux-liveness poll interval (default 10000).
- `TFA_NO_SCAN` — testing/advanced: set to `1` to disable the
  background scanner entirely (hook events only).

## Metric fields in `tfa list`

Beyond the M1 basics (pane, agent, state, task), each session in the
`tfa list` JSON output carries these fields, enriched by the scanner
(`null` until known):

- `source` — how the session is tracked: `hook` (agent hooks only),
  `scan` (discovered by the scanner), or `both` (hook-reported and
  scanner-confirmed).
- `model` — the model the agent is running, as reported by its
  transcript (e.g. `claude-fable-5`).
- `context` — context-window usage: `used_tokens`, `max_tokens`, and
  `percent` full.
- `tokens` — cumulative token totals for the session: `input`,
  `output`, `cache_read`, `cache_creation`, and `total`.
- `git_branch` — the git branch the agent is working on, as reported
  by its transcript.
