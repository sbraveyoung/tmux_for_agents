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

## tfa tui — tmux 内交互仪表盘

全屏 TUI：列表 + 详情两栏，↑↓/jk 选中，Enter 直接把你带到该 agent 的
pane（只切窗口、绝不注入按键），q/Esc/Ctrl-C 退出。数据每 1s 从 daemon
快照刷新；daemon 断连时显示「重连中…」，UI 永不冻结。

推荐键位（`tfa tui --print-keybindings` 可再次打印；tfa 不会自动改你的
tmux.conf，请自行加入 `~/.tmux.conf`）：

    # popup（按需查看；需 tmux >= 3.2）：prefix+a 弹出
    bind a run-shell -b "tmux display-popup -c '#{client_tty}' -t '#{pane_id}' -e TFA_CLIENT='#{client_tty}' -E -w 90% -h 80% 'tfa tui'"
    # 侧栏（任意 tmux 版本）：prefix+A 打开；--stay 让 Enter 跳转后侧栏常驻（跳转已发生，原窗口继续刷新）
    bind A run-shell -b "tmux split-window -t '#{pane_id}' -h -l 40% -e TFA_CLIENT='#{client_tty}' 'tfa tui --stay'"

display-popup/split-window 的 `-e` 不做 format 展开（tmux 3.7b 实测），所以
必须经 `run-shell` 让 `#{client_tty}` 先展开；`tfa` 侧对 `TFA_CLIENT` 也做
健全性检查，配错时自动降级为单 client 模式。`-t '#{pane_id}'` 同样是必须
的——run-shell 里的 tmux 是无 `TMUX_PANE` 的新 CLI client，不显式 `-t` 会
回落到「最近活跃 session」启发式，可能挂错窗口。

`TFA_CLIENT='#{client_tty}'` 是多 client 场景（多个终端窗口 attach 同一
tmux server）下 Enter 跳转能切对 client 的承重配置——popup/split 子进程
本身不是 tmux client，不注入则退化为 tmux 隐式推断，可能切错。

已知行为：

- 嵌套 tmux（SSH 远端再开 tmux）下不保证跳转正确；非 tmux 环境里 Enter
  禁用并提示。
- 多个 client attach **同一** session 时，Enter 跳转会联动所有这些
  client（tmux 会话模型：一个 session 只有一个当前 window，不是 client
  私有的）——要独立观察不同 agent，请让每个 client attach 不同 session。
  这一联动还有一种不那么直观的触发方式：跳转目标恰好落在**另一个**
  client 正在看的 session 里时，那个 client 的当前 window 也会跟着变——
  即使发起跳转的你和它本来 attach 的不是同一个 session。多 client 各自
  独立视图，推荐用 grouped session（共享窗口集合，但各 session 的当前
  window 各自独立）：`tmux new-session -t <session> -s <name>` 为每个
  client 建一个 grouped session 再各自 attach，Enter 跳转就不会互相打扰。
- 死亡 agent 的 pane 若仍存在，Enter 仍会跳转过去（导航目标是 pane 不是
  进程，方便看现场输出或重启）；pane 本身消失才会报「该会话已结束，
  刷新中…」。
- daemon 被杀（如 `pkill tfa` 或崩溃）会在下一次轮询请求时自动拉起
  （`client::request` 的 autospawn 兜底），自愈通常快到 Footer 都来不及
  显示「重连中…」就已经重新连上。想实际观察断连态（比如确认 Footer
  文案），用 `TFA_NO_SPAWN=1 tfa tui` 关掉自动拉起。

### 外观：`[tui]` 配置（可选）

默认黑白（仅结构样式：等待行粗体、已退出行灰显、选中行反显——不依赖
颜色区分状态）。彩色需要在 config.toml（默认 `~/.config/tfa/config.toml`，
可用 `TFA_CONFIG_PATH` 覆盖，见下方 Environment variables）里显式开启：

```toml
[tui]
color = true              # 默认 false（黑白）

[tui.state_colors]        # 可选，覆盖内置调色板；未知颜色名→忽略、回退默认
waiting = "magenta"       # 支持 black/red/green/yellow/blue/magenta/cyan/white/
                           # gray|grey/darkgray|darkgrey/light 前缀系列，大小写不敏感
```

`color = true` 且未覆盖时的内置调色板：等待中 cyan+粗体、工作中 green、
启动中/已完成沿用终端默认色、失联 magenta、已退出 darkgray。等待行的
粗体无论是否开启颜色都保留（紧急度信号不应该只靠颜色传达）。

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
  `~/.config/tfa/config.toml`). The daemon reads this once at
  startup; a missing file or bad TOML silently falls back to
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
