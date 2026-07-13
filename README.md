English | [简体中文](README.zh-CN.md)

# tfa — tmux for agents

AI coding agent observability for tmux: who's working, who's waiting for
you, who's done — in your status bar, and in a full interactive
dashboard when you want the details.

![tfa tui — interactive dashboard](docs/assets/tfa-tui.png)

tfa is a small daemon + a set of agent hooks + a background scanner:

- **hooks** report Claude Code lifecycle events (session start, prompt
  submitted, waiting on you, stopped, tool use) to a local daemon the
  moment they happen;
- a **background scanner** reads agent transcripts/state directly (Claude
  Code's JSONL sessions, Codex's sqlite state) so sessions still show up
  even before hooks fire, and enriches every session with model, context
  usage, and token totals;
- the **daemon** merges both sources into one snapshot per tmux pane and
  serves it over a local Unix socket;
- `tfa status --format tmux` renders a one-line summary
  (`⚡1 ⏸2 ✓1 💀0`) for your tmux `status-right`;
- `tfa tui` renders that same snapshot as a full interactive dashboard
  inside tmux — list + detail panes, keyboard navigation, and the killer
  feature: select an agent and press Enter to jump straight to its pane;
- tfa can also **proactively notify you** (desktop notification, tmux
  status message, and/or a phone push) when an agent starts waiting on
  your input, finishes, goes stale, or dies;
- and it keeps a **local estimate** of your token usage/burn rate per
  provider (not a read of your real subscription quota — see
  [Quota](#quota-tfa-list--tfa-status---format-json) below for exactly
  what that does and doesn't mean).

## Install

### Prerequisites

- A Rust toolchain (`cargo`) — install via [rustup](https://rustup.rs)
  if you don't have one.
- tmux **>= 3.1** (the sidebar keybinding needs 3.1; the popup
  keybinding needs **>= 3.2** for `display-popup`).
- macOS, for the desktop-notification channel (`terminal-notifier` or
  `osascript`) — see [Notifications](#notifications-m3) below. The rest
  of tfa (daemon, hooks, scanner, TUI, tmux/HTTP notification channels)
  uses only portable pieces (Unix sockets, `tmux`, `ps`) and should run
  on Linux, but development and testing so far have been macOS-only —
  treat Linux as untested, and expect desktop notifications specifically
  to be a silent no-op there until someone adds a `notify-send` channel.

### 1. Clone and build

```sh
git clone https://github.com/sbraveyoung/tmux_for_agents
cd tmux_for_agents
cargo install --path .
```

This installs the `tfa` binary to `~/.cargo/bin` (make sure that's on
your `PATH`).

### 2. Claude Code integration

Inside a Claude Code session:

```
/plugin marketplace add sbraveyoung/tmux_for_agents
/plugin install tfa@tfa
```

This wires up the hooks (session start/end, prompt submit, notification,
stop, post-tool-use) that report to the tfa daemon. New Claude Code
sessions pick them up automatically; restart already-running sessions
(or run `claude -c` to resume one) for them to take effect.

### 3. tmux.conf

Add a status-bar snippet:

```tmux
set -g status-interval 5
set -g status-right '#(tfa status --format tmux) | %H:%M'
```

And the recommended `tfa tui` keybindings (also printable any time via
`tfa tui --print-keybindings`; tfa never edits your `tmux.conf` for you):

```tmux
# ~/.tmux.conf — recommended tfa tui keybindings
# ~/.tmux.conf — tfa tui 推荐键位
# Note: display-popup/split-window's -e does not expand tmux formats (verified on tmux 3.7b);
# 注意：display-popup/split-window 的 -e 不做 format 展开（tmux 3.7b 实测），
# you must wrap with run-shell so #{client_tty} expands to a real tty before injection.
# 必须用 run-shell 包装，让 #{client_tty} 在按键时先展开成真实 tty 再注入。
# popup (on demand; needs tmux >= 3.2): prefix+a opens it, q/Esc closes, Enter-jump auto-closes it
# popup（按需查看；需 tmux >= 3.2）：prefix+a 弹出，q/Esc 关闭，Enter 跳转后自动关闭
bind a run-shell -b "tmux display-popup -c '#{client_tty}' -t '#{pane_id}' -e TFA_CLIENT='#{client_tty}' -E -w 90% -h 80% 'tfa tui'"
# sidebar (needs tmux >= 3.1): prefix+A opens it; --stay keeps it resident after Enter jumps (the jump already happened; the original window keeps refreshing)
# 侧栏（需 tmux >= 3.1）：prefix+A 打开；--stay 让 Enter 跳转后侧栏常驻（跳转已发生，原窗口继续刷新）
bind A run-shell -b "tmux split-window -t '#{pane_id}' -h -l 40% -e TFA_CLIENT='#{client_tty}' 'tfa tui --stay'"
```

New claude sessions appear in both the status bar and the TUI
automatically. Existing sessions appear after their next prompt, or
restart them with `claude -c`.

### 4. Optional `~/.config/tfa/config.toml`

Everything below is optional — a missing file (or `TFA_CONFIG_PATH`
pointing nowhere) means all defaults, never a hard error. The `[tui]`
table controls the dashboard's language/appearance:

```toml
[tui]
lang = "auto"             # "auto" (default, detects via LANG/LC_*) | "en" | "zh"
color = false              # default false (monochrome); true enables the palette below
[tui.state_colors]         # optional, only used when color = true
waiting = "magenta"        # overrides one state's color; see README section below for the full list
```

Notification and quota-window settings (`[notify]`, `[quota]`) live in
the same file — see [Notifications](#notifications-m3) and
[Quota](#quota-tfa-list--tfa-status---format-json) below for their full
schemas.

## `tfa tui` — interactive dashboard inside tmux

Full-screen TUI: list + detail two-pane layout, ↑↓/jk to select, Enter
jumps you straight to that agent's pane (only switches window — never
injects keystrokes into it), q/Esc/Ctrl-C to quit. Data refreshes from
the daemon's snapshot every 1s; the footer shows "connected·just now" /
"connected·Ns ago" to indicate snapshot freshness (under 2s counts as
"just now", exact seconds after that), "reconnecting…" while the daemon
is unreachable — the UI never freezes.

The session column shows the precise coordinate `session:window.pane`
(e.g. `company:3.0`) so that multiple agents in different windows/panes
of the *same* tmux session are still distinguishable at a glance; it
falls back to `session %pane_id` when the coordinate isn't known yet.

The UI is bilingual (English/Chinese): it auto-detects your language
from `LC_ALL`/`LC_MESSAGES`/`LANG` (first non-empty wins) at startup,
overridable via `[tui] lang = "en" | "zh"` in `config.toml` (see above).

Known behaviors:

- **Nested tmux** (e.g. SSH into a remote box that also runs tmux)
  doesn't guarantee correct jumps; outside any tmux session, Enter is
  disabled and shows a hint instead.
- When **multiple clients are attached to the same session**, Enter's
  jump affects all of those clients (tmux's session model: a session
  has one current window, not a per-client one) — to observe different
  agents independently, attach each client to a different session. A
  less obvious variant of the same thing: if the jump target happens to
  land in a session that *another* client is currently looking at, that
  client's current window changes too, even if you two didn't start out
  attached to the same session. For fully independent multi-client
  views, use grouped sessions (they share the same window set, but each
  has its own independent "current window"):
  `tmux new-session -t <session> -s <name>` per client, then attach each
  one separately — Enter jumps then won't step on each other.
- If a **dead agent's pane still exists**, Enter still jumps to it (the
  navigation target is the pane, not the process, so you can inspect
  the last output or restart it there); only once the pane itself is
  gone does it report "session ended, refreshing…".
- If the **daemon is killed** (e.g. `pkill tfa`, or a crash), it's
  auto-respawned on the next poll (`client::request`'s autospawn
  fallback) — recovery is usually faster than the footer even has a
  chance to show "reconnecting…". To actually observe the disconnected
  state (e.g. to confirm the footer text), disable autospawn with
  `TFA_NO_SPAWN=1 tfa tui`.

### Appearance: `[tui]` config (optional)

Monochrome by default (structural styling only: the waiting row is
bold, the exited row is dimmed gray, the selected row is reverse-video —
no color is required to tell states apart). Color needs to be turned on
explicitly in `config.toml` (default
`~/.config/tfa/config.toml`, overridable with `TFA_CONFIG_PATH`, see
Environment variables below):

```toml
[tui]
color = true              # default false (monochrome)

[tui.state_colors]        # optional, overrides the built-in palette; unknown color names are ignored
waiting = "magenta"       # supports black/red/green/yellow/blue/magenta/cyan/white/
                           # gray|grey/darkgray|darkgrey/light-prefixed variants, case-insensitive
```

Built-in palette when `color = true` and a state isn't overridden:
waiting = cyan+bold, working = green, starting/done = terminal default,
stale = magenta, exited = darkgray. The waiting row's bold styling is
kept regardless of `color` (urgency shouldn't rely on color alone).

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
- `TFA_CONFIG_PATH` — override the config file path (default
  `~/.config/tfa/config.toml`). The daemon (and `tfa tui`) reads this
  once at startup; a missing file or bad TOML silently falls back to
  defaults (never a hard error).

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
- `TFA_NO_NOTIFY` — testing/advanced: set to `1` to suppress real
  notification dispatch (no terminal-notifier/osascript/tmux/HTTP
  calls). Instead each event is appended as a JSON line to
  `state_dir/notify-sink.jsonl`, for e2e tests and debugging to
  inspect what *would* have been sent.

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

## Quota (`tfa list` / `tfa status --format json`)

Alongside `sessions`, the JSON output carries a top-level `quota`
array — one entry per provider (`claude`, `codex`) that has produced
token activity, e.g.:

```json
{
  "sessions": [ ... ],
  "quota": [
    {
      "provider": "claude",
      "window_5h_percent": null,
      "weekly_percent": null,
      "reset_at_ms": 1770000000000,
      "reset_estimated": true,
      "observed_tokens_this_window": 128400,
      "burn_rate_per_min": 812.5,
      "source": "local_estimate",
      "freshness_ms": 1769999999000
    }
  ]
}
```

This is a **local estimate**, not a read of your real subscription
quota — M3 has no access to Anthropic's actual usage API. Read it
accordingly:

- `observed_tokens_this_window` — tokens tfa has *itself observed*
  flowing through that provider's sessions in the current rolling
  5h window (input + output, cache reads excluded). It is a **lower
  bound**, not your subscription's remaining/used quota — tfa only
  sees activity while its daemon was running and a session was
  tracked.
- `burn_rate_per_min` — tokens/minute, derived purely from the same
  observed stream (see `[quota] burn_rate_window_mins` below).
- `source` — always `"local_estimate"` in M3. There is no other
  value yet.
- `window_5h_percent` and `weekly_percent` — always `null` in M3.
  tfa has no way to know your plan's real limit, so it never
  fabricates a percentage. Real `%` (backed by Anthropic's
  usage endpoint) is deferred to a future "real quota" milestone;
  `reset_at_ms`/`reset_estimated` are its estimated placeholders in
  the meantime (`reset_estimated` is always `true` in M3).

## Notifications (M3)

tfa can proactively notify you — desktop notification, tmux status
message, and/or a phone push — when a session starts waiting on
your input, finishes, goes stale, or dies. Configure it in
`~/.config/tfa/config.toml` (created by hand; tfa never writes it),
overridable with `TFA_CONFIG_PATH`. Everything below is optional —
a missing file means all defaults (`waiting_input` notifications on,
via tmux + macOS, everything else off).

```toml
[notify]
enabled = true
# Optional quiet hours: silence waiting_input/done/stale in this window.
# `dead` always gets through (a real crash should never be swallowed).
# quiet_hours = { start = "23:00", end = "08:00" }
# quiet_hours_exempt = ["dead"]   # default exempt set

[notify.channels.tmux]   # zero-cost (no extra process), on by default
enabled = true
[notify.channels.macos]  # desktop notification, on by default
enabled = true
[notify.channels.http]   # phone push via Bark/ntfy, off by default
enabled = false
url = ""            # see "Phone push (Bark/ntfy)" below
format = "bark"      # bark | ntfy | generic-json
timeout_ms = 3000    # hard cap on the HTTP call (max ~10000)
headers = {}         # e.g. ntfy access-control headers, webhook auth

[notify.triggers]
waiting_input = true      # agent needs your permission/input
done          = false     # agent finished a turn (off by default: noisy)
stale         = false     # agent has gone quiet without finishing
dead          = false     # agent process is gone

[notify.discipline]
cooldown_secs       = 30  # per-(session, kind) edge cooldown
dead_debounce_ticks = 2   # consecutive dead scans required before notifying
boot_grace_secs     = 30  # suppress notifications for this long after daemon start
                           # (so restoring old sessions from the snapshot doesn't
                           # fire a burst of stale notifications)

[quota]
burn_rate_window_mins = 60  # rolling window used for burn_rate_per_min above
```

### Trying it: `tfa notify test`

    tfa notify test

Sends one test notification through every currently-enabled channel.
Use it after editing `config.toml` to confirm each channel actually
reaches you before relying on it.

**macOS first run**: tfa prefers
[`terminal-notifier`](https://github.com/julienXX/terminal-notifier)
if it's on `PATH` (`brew install terminal-notifier`) — it registers
as its own app, so its notification permission is independent and
`-title`/`-sound` etc. are reliable. Without it, tfa falls back to
`osascript` (`display notification`), which shows up under
**Script Editor**'s (or **Event Monitor**'s, on newer macOS)
notification permission instead of `tfa`'s own. Either way, the
*first* notification usually triggers a macOS permission prompt; if
you miss it or already denied it, grant it manually in
**System Settings → Notifications → terminal-notifier (or Script
Editor)**. Note the sharp edge: a denied `osascript` notification
fails **silently** — the process still exits 0, tfa has no way to
detect it didn't show up, and it will not retry or warn you.

### Phone push (Bark/ntfy)

`[notify.channels.http]` POSTs a JSON payload to any URL, with two
built-in shapes:

- **Bark** (`format = "bark"`): `url` is your Bark server + device
  key (e.g. `https://your-bark-server/AbCdEfG`); tfa POSTs to
  `<server>/push` with `device_key` in the body.
- **ntfy** (`format = "ntfy"`): `url` is the full topic URL (e.g.
  `https://ntfy.sh/your-topic` or your self-hosted equivalent); tfa
  POSTs the JSON payload straight to it.
- **generic-json**: POSTs `{kind, session, pane, title, body}` to
  `url` as-is, for your own webhook.

**Honest caveat, read before you self-host a server expecting to
dodge the internet**: iOS background/lock-screen push notifications
are *always* delivered through Apple's APNs — that's how iOS wakes
your app in the background, full stop. Running your own Bark or
ntfy server does **not** remove the dependency on APNs; it only
changes who handles your notification content in transit (your
server vs. a third party's) before it still has to go out to Apple
and back down to your phone. **There is no pure-LAN, zero-internet
way to get iOS background push** — if your phone and the machine
running `tfa daemon` are both offline from the wider internet, phone
push will not arrive no matter which server you point `url` at. (A
real-time LAN dashboard you actively look at, with no push
involved, is a possible future direction — see the milestone list in
`docs/superpowers/specs/`.)
