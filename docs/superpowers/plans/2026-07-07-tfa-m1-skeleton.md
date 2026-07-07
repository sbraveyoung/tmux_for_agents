# tfa M1 骨架 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 单二进制 `tfa`：常驻 daemon + hook 瘦转发 + 每 pane 状态机 + `tfa status` 状态栏输出，上线即回答「哪个 agent 在干活/等输入/已完成」。

**Architecture:** daemon 监听 unix socket 作为唯一事实来源；`tfa hook` 由 Claude Code hooks 调用、把事件转发到 socket（连不上则拉起一次 daemon，失败静默退出）；状态机纯函数化便于单测；客户端命令从 snapshot 渲染。M1 无 scanner（M2 加），`Stale` 状态仅定义不触发。Subscribe 推送协议 M3/M4 有消费者时再加。

**Tech Stack:** Rust (edition 2021, stable toolchain)。依赖：clap 4 (derive)、serde/serde_json、fd-lock 4、libc、anyhow。dev 依赖：tempfile、assert_cmd、predicates。无 async runtime（std 线程足够）。

**Spec:** `docs/superpowers/specs/2026-07-07-tmux-agent-observability-design.md`

## Global Constraints

- 二进制名 `tfa`；仓库根即 cargo 项目根。
- hook 路径纪律：`tfa hook` 任何失败都必须静默 `exit 0`，I/O 超时 100ms，最多尝试拉起 daemon 一次 —— 绝不阻塞 agent。
- 所有路径可用环境变量覆盖（测试隔离的基础）：`TFA_SOCKET`（socket 路径）、`TFA_STATE_DIR`（快照/锁目录）、`TFA_TMUX_SOCKET`（tmux `-L` 名，空则用默认 server）、`TFA_BIN`（hook.sh 用）。
- 默认路径：socket `/tmp/tfa-<uid>/tfa.sock`；state dir `~/.local/state/tfa`。
- 时间戳统一 epoch 毫秒 `u64`（可序列化、可跨重启），字段后缀 `_ms`。
- 状态机六态：`Starting | Working | WaitingInput{reason} | Done | Dead | Stale`。
- 每个任务：测试先行，全绿才 commit；commit message 用 conventional commits。

---

### Task 1: 项目脚手架与 CLI 骨架

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/paths.rs`, `.gitignore`
- Test: `tests/cli.rs`

**Interfaces:**
- Produces: clap 子命令骨架 `tfa daemon|hook|status|list`；`paths::socket_path() -> PathBuf`、`paths::state_dir() -> PathBuf`、`paths::tmux_args() -> Vec<String>`（后续所有任务消费）。

- [ ] **Step 1: 初始化项目与依赖**

```bash
cargo init --name tfa
```

`Cargo.toml`:

```toml
[package]
name = "tfa"
version = "0.1.0"
edition = "2021"

[dependencies]
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
fd-lock = "4"
libc = "0.2"
anyhow = "1"

[dev-dependencies]
tempfile = "3"
assert_cmd = "2"
predicates = "3"
```

`.gitignore`: `target/`

- [ ] **Step 2: 写失败的 CLI 测试**

`tests/cli.rs`:

```rust
use assert_cmd::Command;

#[test]
fn help_lists_subcommands() {
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    let assert = cmd.arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in ["daemon", "hook", "status", "list"] {
        assert!(out.contains(sub), "missing subcommand {sub}");
    }
}

#[test]
fn status_without_daemon_reports_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1") // 测试用：禁止自动拉起
        .args(["status", "--format", "tmux"]);
    cmd.assert().success().stdout(predicates::str::contains("tfa:off"));
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo test --test cli`
Expected: FAIL（binary 尚无子命令）

- [ ] **Step 4: 实现 CLI 骨架与 paths**

`src/paths.rs`:

```rust
use std::path::PathBuf;

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn socket_path() -> PathBuf {
    env_path("TFA_SOCKET").unwrap_or_else(|| {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/tfa-{uid}/tfa.sock"))
    })
}

pub fn state_dir() -> PathBuf {
    env_path("TFA_STATE_DIR").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".local/state/tfa")
    })
}

pub fn snapshot_path() -> PathBuf { state_dir().join("snapshot.json") }
pub fn lock_path() -> PathBuf { state_dir().join("daemon.lock") }

/// tmux 调用的额外参数（隔离测试用 -L <name>）
pub fn tmux_args() -> Vec<String> {
    match std::env::var("TFA_TMUX_SOCKET") {
        Ok(name) if !name.is_empty() => vec!["-L".into(), name],
        _ => vec![],
    }
}
```

`src/main.rs`:

```rust
mod paths;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tfa", about = "tmux for agents — AI agent observability")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground
    Daemon,
    /// Forward an agent hook event to the daemon (called by agent hooks)
    Hook { agent: String, event: String },
    /// Render current agent states
    Status {
        #[arg(long, default_value = "tmux")]
        format: String,
    },
    /// Dump full state as JSON
    List,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => todo!("task 5"),
        Command::Hook { .. } => std::process::exit(0), // hook 纪律：未实现也静默
        Command::Status { .. } => println!("tfa:off"),
        Command::List => println!("[]"),
    }
}
```

- [ ] **Step 5: 测试通过后提交**

Run: `cargo test --test cli`
Expected: PASS (2 tests)

```bash
git add Cargo.toml Cargo.lock .gitignore src/ tests/
git commit -m "feat: tfa CLI skeleton with paths module"
```

---

### Task 2: 协议与事件类型（protocol.rs / event.rs）

**Files:**
- Create: `src/protocol.rs`, `src/event.rs`
- Modify: `src/main.rs`（加 `mod protocol; mod event;`）

**Interfaces:**
- Produces:
  - `protocol::Request { Hook { agent, event, pane, payload }, Snapshot }`（serde，`op` tag，snake_case）
  - `protocol::Response { Ok, Snapshot { sessions, generated_at_ms }, Error { message } }`
  - `event::AgentKind { Claude, Codex, Custom(String) }`
  - `event::EventKind { SessionStart, UserPromptSubmit, Notification, Stop, SessionEnd, Activity }`
  - `event::AgentEvent::from_hook(agent: &str, event: &str, pane: &str, payload: &serde_json::Value, at_ms: u64) -> Option<AgentEvent>`
  - `AgentEvent` 字段：`agent, pane_id, kind, reason: Option<String>, prompt: Option<String>, cwd: Option<String>, at_ms`

- [ ] **Step 1: 写失败的单测（文件内 `#[cfg(test)]`）**

`src/event.rs` 测试部分：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_notification_maps_to_waiting_reason() {
        let payload = json!({
            "session_id": "abc", "cwd": "/tmp/p",
            "message": "Claude needs your permission to use Bash"
        });
        let ev = AgentEvent::from_hook("claude", "notification", "%7", &payload, 1000).unwrap();
        assert!(matches!(ev.kind, EventKind::Notification));
        assert_eq!(ev.reason.as_deref(), Some("Claude needs your permission to use Bash"));
        assert_eq!(ev.pane_id, "%7");
        assert_eq!(ev.cwd.as_deref(), Some("/tmp/p"));
    }

    #[test]
    fn prompt_is_truncated_to_120_chars() {
        let long = "x".repeat(300);
        let payload = json!({ "prompt": long });
        let ev = AgentEvent::from_hook("claude", "user-prompt-submit", "%1", &payload, 0).unwrap();
        assert_eq!(ev.prompt.as_ref().unwrap().chars().count(), 120);
    }

    #[test]
    fn unknown_event_returns_none_and_unknown_agent_is_custom() {
        assert!(AgentEvent::from_hook("claude", "no-such-event", "%1", &serde_json::Value::Null, 0).is_none());
        let ev = AgentEvent::from_hook("hermes", "stop", "%1", &serde_json::Value::Null, 0).unwrap();
        assert!(matches!(ev.agent, AgentKind::Custom(ref n) if n == "hermes"));
    }
}
```

`src/protocol.rs` 测试部分：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let line = r#"{"op":"hook","agent":"claude","event":"stop","pane":"%3","payload":{}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert!(matches!(req, Request::Hook { .. }));
        let back = serde_json::to_string(&req).unwrap();
        assert!(back.contains(r#""op":"hook""#));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test`
Expected: FAIL（类型不存在，编译错误）

- [ ] **Step 3: 实现**

`src/protocol.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hook {
        agent: String,
        event: String,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        payload: serde_json::Value,
    },
    Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Snapshot {
        sessions: Vec<crate::state::AgentSession>,
        generated_at_ms: u64,
    },
    Error { message: String },
}
```

`src/event.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Custom(String),
}

impl AgentKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            other => Self::Custom(other.to_string()),
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Custom(n) => n,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventKind {
    SessionStart,
    UserPromptSubmit,
    Notification,
    Stop,
    SessionEnd,
    Activity,
}

impl EventKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "session-start" => Self::SessionStart,
            "user-prompt-submit" => Self::UserPromptSubmit,
            "notification" => Self::Notification,
            "stop" => Self::Stop,
            "session-end" => Self::SessionEnd,
            "activity" | "post-tool-use" => Self::Activity,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub agent: AgentKind,
    pub pane_id: String,
    pub kind: EventKind,
    pub reason: Option<String>,
    pub prompt: Option<String>,
    pub cwd: Option<String>,
    pub at_ms: u64,
}

fn str_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key)?.as_str().map(|s| s.to_string())
}

impl AgentEvent {
    pub fn from_hook(agent: &str, event: &str, pane: &str, payload: &Value, at_ms: u64) -> Option<Self> {
        let kind = EventKind::parse(event)?;
        Some(Self {
            agent: AgentKind::parse(agent),
            pane_id: pane.to_string(),
            kind,
            reason: str_field(payload, "message"),
            prompt: str_field(payload, "prompt").map(|p| p.chars().take(120).collect()),
            cwd: str_field(payload, "cwd"),
            at_ms,
        })
    }
}
```

`src/main.rs` 顶部加：`mod event; mod protocol; mod state;`（`state` 模块 Task 3 创建；本任务先建空文件 `src/state.rs` 内容为 Task 3 的类型占位 —— 为避免占位，本任务 `protocol.rs` 的 `Response::Snapshot` 暂用 `Vec<serde_json::Value>`，Task 3 再改回 `Vec<AgentSession>`）。

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（4 个新测试 + Task 1 的 2 个）

```bash
git add src/
git commit -m "feat: wire protocol and agent event types"
```

---

### Task 3: 状态机（state.rs）—— 核心逻辑

**Files:**
- Create: `src/state.rs`
- Modify: `src/protocol.rs`（`Snapshot.sessions` 改为 `Vec<AgentSession>`）

**Interfaces:**
- Consumes: `event::{AgentEvent, AgentKind, EventKind}`
- Produces:
  - `state::SessionState { Starting, Working, WaitingInput { reason: String }, Done, Dead, Stale }`（serde tag=`state`）
  - `state::AgentSession { pane_id, agent: AgentKind, session_name: Option<String>, state: SessionState, state_since_ms: u64, current_task: Option<String>, cwd: Option<String>, last_activity_ms: u64 }`（全字段 pub + Serialize/Deserialize/Clone）
  - `state::StateStore::new() / apply(&mut self, ev: AgentEvent) / prune(&mut self, now_ms: u64) / sessions(&self) -> Vec<AgentSession> / to_json(&self) -> String / from_json(&str) -> anyhow::Result<StateStore> / set_session_name(&mut self, pane_id: &str, name: String)`
  - 常量 `state::DEAD_RETENTION_MS: u64 = 300_000`

- [ ] **Step 1: 写失败的状态机单测**

`src/state.rs` 的 `#[cfg(test)]`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentKind, EventKind};

    fn ev(kind: EventKind, at_ms: u64) -> AgentEvent {
        AgentEvent {
            agent: AgentKind::Claude, pane_id: "%1".into(), kind,
            reason: None, prompt: None, cwd: None, at_ms,
        }
    }

    #[test]
    fn lifecycle_start_prompt_stop() {
        let mut s = StateStore::new();
        s.apply(ev(EventKind::SessionStart, 1000));
        assert!(matches!(s.sessions()[0].state, SessionState::Starting));

        let mut e = ev(EventKind::UserPromptSubmit, 2000);
        e.prompt = Some("fix the bug".into());
        s.apply(e);
        let sess = &s.sessions()[0];
        assert!(matches!(sess.state, SessionState::Working));
        assert_eq!(sess.state_since_ms, 2000);
        assert_eq!(sess.current_task.as_deref(), Some("fix the bug"));

        s.apply(ev(EventKind::Stop, 3000));
        assert!(matches!(s.sessions()[0].state, SessionState::Done));
    }

    #[test]
    fn notification_sets_waiting_with_reason() {
        let mut s = StateStore::new();
        s.apply(ev(EventKind::SessionStart, 0));
        let mut e = ev(EventKind::Notification, 100);
        e.reason = Some("needs permission".into());
        s.apply(e);
        match &s.sessions()[0].state {
            SessionState::WaitingInput { reason } => assert_eq!(reason, "needs permission"),
            other => panic!("expected WaitingInput, got {other:?}"),
        }
    }

    #[test]
    fn event_for_unknown_pane_creates_entry() {
        // 老会话没发过 session-start，任何事件都要建档（sidebar 盲区的教训）
        let mut s = StateStore::new();
        s.apply(ev(EventKind::UserPromptSubmit, 500));
        assert_eq!(s.sessions().len(), 1);
        assert!(matches!(s.sessions()[0].state, SessionState::Working));
    }

    #[test]
    fn activity_touches_last_activity_but_keeps_waiting_state() {
        let mut s = StateStore::new();
        let mut e = ev(EventKind::Notification, 100);
        e.reason = Some("input".into());
        s.apply(e);
        s.apply(ev(EventKind::Activity, 200));
        let sess = &s.sessions()[0];
        assert_eq!(sess.last_activity_ms, 200);
        assert!(matches!(sess.state, SessionState::WaitingInput { .. }));
    }

    #[test]
    fn session_end_marks_dead_and_prune_removes_after_retention() {
        let mut s = StateStore::new();
        s.apply(ev(EventKind::SessionStart, 0));
        s.apply(ev(EventKind::SessionEnd, 1000));
        assert!(matches!(s.sessions()[0].state, SessionState::Dead));
        s.prune(1000 + DEAD_RETENTION_MS - 1);
        assert_eq!(s.sessions().len(), 1);
        s.prune(1000 + DEAD_RETENTION_MS + 1);
        assert_eq!(s.sessions().len(), 0);
    }

    #[test]
    fn json_roundtrip_preserves_state() {
        let mut s = StateStore::new();
        s.apply(ev(EventKind::SessionStart, 0));
        s.set_session_name("%1", "myproj".into());
        let restored = StateStore::from_json(&s.to_json()).unwrap();
        assert_eq!(restored.sessions()[0].session_name.as_deref(), Some("myproj"));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test state`
Expected: FAIL（编译错误）

- [ ] **Step 3: 实现状态机**

`src/state.rs`:

```rust
use crate::event::{AgentEvent, AgentKind, EventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEAD_RETENTION_MS: u64 = 300_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Working,
    WaitingInput { reason: String },
    Done,
    Dead,
    Stale, // M1 定义不触发；M2 scanner 纠偏时使用
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub pane_id: String,
    pub agent: AgentKind,
    pub session_name: Option<String>,
    #[serde(flatten)]
    pub state: SessionState,
    pub state_since_ms: u64,
    pub current_task: Option<String>,
    pub cwd: Option<String>,
    pub last_activity_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct StateStore {
    sessions: BTreeMap<String, AgentSession>,
}

impl StateStore {
    pub fn new() -> Self { Self::default() }

    pub fn apply(&mut self, ev: AgentEvent) {
        let entry = self.sessions.entry(ev.pane_id.clone()).or_insert_with(|| AgentSession {
            pane_id: ev.pane_id.clone(),
            agent: ev.agent.clone(),
            session_name: None,
            state: SessionState::Starting,
            state_since_ms: ev.at_ms,
            current_task: None,
            cwd: None,
            last_activity_ms: ev.at_ms,
        });
        entry.last_activity_ms = ev.at_ms;
        if let Some(cwd) = ev.cwd { entry.cwd = Some(cwd); }

        let next = match ev.kind {
            EventKind::SessionStart => Some(SessionState::Starting),
            EventKind::UserPromptSubmit => {
                if let Some(p) = ev.prompt { entry.current_task = Some(p); }
                Some(SessionState::Working)
            }
            EventKind::Notification => Some(SessionState::WaitingInput {
                reason: ev.reason.unwrap_or_else(|| "waiting for input".into()),
            }),
            EventKind::Stop => Some(SessionState::Done),
            EventKind::SessionEnd => Some(SessionState::Dead),
            EventKind::Activity => None, // 只刷新 last_activity
        };
        if let Some(state) = next {
            entry.state = state;
            entry.state_since_ms = ev.at_ms;
        }
    }

    pub fn prune(&mut self, now_ms: u64) {
        self.sessions.retain(|_, s| {
            !(matches!(s.state, SessionState::Dead)
                && now_ms.saturating_sub(s.state_since_ms) > DEAD_RETENTION_MS)
        });
    }

    pub fn set_session_name(&mut self, pane_id: &str, name: String) {
        if let Some(s) = self.sessions.get_mut(pane_id) {
            s.session_name = Some(name);
        }
    }

    pub fn sessions(&self) -> Vec<AgentSession> {
        self.sessions.values().cloned().collect()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("state serializes")
    }

    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}
```

`src/protocol.rs`：把 `Snapshot` 变体改为

```rust
    Snapshot {
        sessions: Vec<crate::state::AgentSession>,
        generated_at_ms: u64,
    },
```

注意 `SessionState` 用了 `#[serde(flatten)]` + tag，JSON 形如
`{"pane_id":"%1","state":"waiting_input","reason":"..."}` —— Task 7 渲染和 Task 9 断言都依赖这个形状。

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（新增 6 个状态机测试）

```bash
git add src/
git commit -m "feat: per-pane agent session state machine"
```

---

### Task 4: daemon 核心 —— socket 服务与请求处理

**Files:**
- Create: `src/daemon/mod.rs`, `src/daemon/server.rs`
- Modify: `src/main.rs`（`mod daemon;`，`Command::Daemon` 接入）
- Test: `tests/daemon_socket.rs`

**Interfaces:**
- Consumes: `protocol::{Request, Response}`、`state::StateStore`、`paths::*`
- Produces:
  - `daemon::run() -> anyhow::Result<()>`（前台运行，Task 5 补生命周期）
  - `daemon::server::serve(listener: UnixListener, store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>)`（accept 循环；每连接一线程；JSON-lines：一行请求→一行响应）
  - 现在时间函数 `daemon::now_ms() -> u64`（`SystemTime` epoch ms，全项目唯一取时点）

- [ ] **Step 1: 写失败的 socket 集成测试**

`tests/daemon_socket.rs`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) { let _ = self.0.kill(); }
}

fn start_daemon(dir: &std::path::Path) -> (DaemonGuard, std::path::PathBuf) {
    let sock = dir.join("tfa.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir)
        .env("TFA_SKIP_TMUX_CHECK", "1") // 测试环境无 tmux
        .arg("daemon")
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    (DaemonGuard(child), sock)
}

fn roundtrip(sock: &std::path::Path, line: &str) -> String {
    let mut s = UnixStream::connect(sock).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    let mut reader = BufReader::new(s);
    let mut resp = String::new();
    reader.read_line(&mut resp).unwrap();
    resp
}

#[test]
fn hook_then_snapshot_reflects_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_guard, sock) = start_daemon(dir.path());

    let ok = roundtrip(&sock,
        r#"{"op":"hook","agent":"claude","event":"user-prompt-submit","pane":"%9","payload":{"prompt":"hi"}}"#);
    assert!(ok.contains(r#""result":"ok""#), "got: {ok}");

    let snap = roundtrip(&sock, r#"{"op":"snapshot"}"#);
    assert!(snap.contains(r#""pane_id":"%9""#), "got: {snap}");
    assert!(snap.contains(r#""state":"working""#), "got: {snap}");
}

#[test]
fn malformed_request_returns_error_not_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (_guard, sock) = start_daemon(dir.path());
    let resp = roundtrip(&sock, "not json at all");
    assert!(resp.contains(r#""result":"error""#));
    // daemon 仍活着
    let snap = roundtrip(&sock, r#"{"op":"snapshot"}"#);
    assert!(snap.contains("snapshot"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test daemon_socket`
Expected: FAIL（daemon 子命令是 todo!）

- [ ] **Step 3: 实现 daemon 核心**

`src/daemon/mod.rs`:

```rust
pub mod server;

use crate::paths;
use crate::state::StateStore;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn run() -> anyhow::Result<()> {
    let sock_path = paths::socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(paths::state_dir())?;
    let _ = std::fs::remove_file(&sock_path); // 遗留 socket 清理（flock 保证无双活，Task 5）

    let store = Arc::new(Mutex::new(load_or_default()));
    let dirty = Arc::new(AtomicBool::new(false));

    let listener = UnixListener::bind(&sock_path)?;
    server::serve(listener, store, dirty); // 阻塞 accept 循环
    Ok(())
}

fn load_or_default() -> StateStore {
    std::fs::read_to_string(paths::snapshot_path())
        .ok()
        .and_then(|s| StateStore::from_json(&s).ok())
        .unwrap_or_else(StateStore::new)
}
```

`src/daemon/server.rs`:

```rust
use crate::event::AgentEvent;
use crate::protocol::{Request, Response};
use crate::state::StateStore;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub fn serve(listener: UnixListener, store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let store = Arc::clone(&store);
        let dirty = Arc::clone(&dirty);
        std::thread::spawn(move || handle(stream, store, dirty));
    }
}

fn handle(stream: UnixStream, store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() { continue; }
        let resp = respond(&line, &store, &dirty);
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() { return; }
    }
}

fn respond(line: &str, store: &Mutex<StateStore>, dirty: &AtomicBool) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Response::Error { message: format!("bad request: {e}") },
    };
    match req {
        Request::Hook { agent, event, pane, payload } => {
            let Some(pane) = pane else {
                return Response::Error { message: "missing pane".into() };
            };
            match AgentEvent::from_hook(&agent, &event, &pane, &payload, super::now_ms()) {
                Some(ev) => {
                    store.lock().unwrap().apply(ev);
                    dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
                None => Response::Error { message: format!("unknown event: {event}") },
            }
        }
        Request::Snapshot => {
            let sessions = store.lock().unwrap().sessions();
            Response::Snapshot { sessions, generated_at_ms: super::now_ms() }
        }
    }
}
```

`src/main.rs`：`Command::Daemon => { if let Err(e) = daemon::run() { eprintln!("tfa daemon: {e}"); std::process::exit(1); } }`

（`TFA_SKIP_TMUX_CHECK` 在 Task 5 生效；此处测试先传着，无害。）

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（新增 2 个 socket 测试）

```bash
git add src/
git commit -m "feat: daemon socket server with hook/snapshot ops"
```

---

### Task 5: daemon 生命周期 —— flock 单例、周期快照、tmux 存活检查

**Files:**
- Modify: `src/daemon/mod.rs`
- Create: `src/daemon/lifecycle.rs`
- Test: `tests/daemon_lifecycle.rs`

**Interfaces:**
- Consumes: Task 4 的 `run()` 结构
- Produces:
  - `run()` 增强：启动时 `try_lock` 失败→已有 daemon→静默 exit 0；每 5s 后台线程：`prune` + dirty 时写快照（原子写：临时文件+rename）；每 10s `tmux has-session` 失败→删 socket→exit 0
  - `lifecycle::acquire_lock() -> Option<fd_lock::RwLockWriteGuard<'static, std::fs::File>>`（None = 已有实例）
  - `lifecycle::tmux_alive() -> bool`（`TFA_SKIP_TMUX_CHECK=1` 时恒 true）
  - `lifecycle::write_snapshot(store: &Mutex<StateStore>)`

- [ ] **Step 1: 写失败的生命周期测试**

`tests/daemon_lifecycle.rs`:

```rust
use std::process::Command;
use std::time::Duration;

fn daemon_cmd(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tfa"));
    c.env("TFA_SOCKET", dir.join("tfa.sock"))
        .env("TFA_STATE_DIR", dir)
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .arg("daemon");
    c
}

#[test]
fn second_daemon_exits_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = daemon_cmd(dir.path()).spawn().unwrap();
    let sock = dir.path().join("tfa.sock");
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    // 第二个实例应拿不到锁，静默退出 0
    let status = daemon_cmd(dir.path()).status().unwrap();
    assert!(status.success());
    // 第一个还活着
    assert!(first.try_wait().unwrap().is_none());
    first.kill().unwrap();
}

#[test]
fn daemon_exits_when_tmux_dead() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_tfa"));
    c.env("TFA_SOCKET", dir.path().join("tfa.sock"))
        .env("TFA_STATE_DIR", dir.path())
        // 指向必然不存在的 tmux server；不设 SKIP
        .env("TFA_TMUX_SOCKET", "tfa-test-definitely-absent")
        .env("TFA_TMUX_CHECK_INTERVAL_MS", "100") // 测试加速
        .arg("daemon");
    let mut child = c.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon didn't exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn snapshot_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    // 直接预置快照文件，daemon 启动应加载
    std::fs::write(
        dir.path().join("snapshot.json"),
        r#"{"sessions":{"%5":{"pane_id":"%5","agent":"claude","session_name":"proj",
            "state":"done","state_since_ms":1,"current_task":null,"cwd":null,"last_activity_ms":1}}}"#,
    ).unwrap();
    let mut child = daemon_cmd(dir.path()).spawn().unwrap();
    let sock = dir.path().join("tfa.sock");
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    s.write_all(b"{\"op\":\"snapshot\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    assert!(line.contains(r#""pane_id":"%5""#), "got: {line}");
    child.kill().unwrap();
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test daemon_lifecycle`
Expected: `second_daemon_exits_immediately` FAIL（第二实例会 bind 失败 exit 1 或抢占 socket）；`daemon_exits_when_tmux_dead` FAIL（不会退出，超时）

- [ ] **Step 3: 实现生命周期**

`src/daemon/lifecycle.rs`:

```rust
use crate::paths;
use crate::state::StateStore;
use std::fs::File;
use std::sync::Mutex;

/// 返回持有的锁 guard；None 表示已有 daemon 在跑。
/// 泄漏 File 换 'static 生命周期：daemon 进程整个生命周期都要持锁，进程退出自动释放。
pub fn acquire_lock() -> Option<fd_lock::RwLockWriteGuard<'static, File>> {
    let path = paths::lock_path();
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = File::create(&path).ok()?;
    let lock: &'static mut fd_lock::RwLock<File> =
        Box::leak(Box::new(fd_lock::RwLock::new(file)));
    lock.try_write().ok()
}

pub fn tmux_alive() -> bool {
    if std::env::var("TFA_SKIP_TMUX_CHECK").as_deref() == Ok("1") {
        return true;
    }
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(paths::tmux_args()).arg("has-session");
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

pub fn write_snapshot(store: &Mutex<StateStore>) {
    let json = store.lock().unwrap().to_json();
    let path = paths::snapshot_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path); // 原子替换，避免半写快照
    }
}

pub fn check_interval_ms() -> u64 {
    std::env::var("TFA_TMUX_CHECK_INTERVAL_MS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(10_000)
}
```

`src/daemon/mod.rs` 的 `run()` 改为：

```rust
pub mod lifecycle;
pub mod server;
// … now_ms、load_or_default 不变 …

pub fn run() -> anyhow::Result<()> {
    std::fs::create_dir_all(paths::state_dir())?;
    let Some(_lock) = lifecycle::acquire_lock() else {
        return Ok(()); // 已有实例，静默退出 0
    };
    let sock_path = paths::socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&sock_path); // 持锁后清理遗留 socket 是安全的

    let store = Arc::new(Mutex::new(load_or_default()));
    let dirty = Arc::new(AtomicBool::new(false));

    // 后台维护线程：快照 + prune（5s）、tmux 存活（可配置，默认 10s）
    {
        let store = Arc::clone(&store);
        let dirty = Arc::clone(&dirty);
        let sock = sock_path.clone();
        std::thread::spawn(move || {
            let check_every = lifecycle::check_interval_ms();
            let mut since_check: u64 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(check_every.min(5000)));
                since_check += check_every.min(5000);
                store.lock().unwrap().prune(now_ms());
                if dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    lifecycle::write_snapshot(&store);
                }
                if since_check >= check_every {
                    since_check = 0;
                    if !lifecycle::tmux_alive() {
                        lifecycle::write_snapshot(&store);
                        let _ = std::fs::remove_file(&sock);
                        std::process::exit(0);
                    }
                }
            }
        });
    }

    let listener = UnixListener::bind(&sock_path)?;
    server::serve(listener, store, dirty);
    Ok(())
}
```

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（新增 3 个生命周期测试；全部既有测试仍绿）

```bash
git add src/ tests/
git commit -m "feat: daemon lifecycle - flock singleton, snapshots, tmux liveness"
```

---

### Task 6: 客户端与 `tfa hook` —— 100ms 纪律与自动拉起

**Files:**
- Create: `src/client.rs`, `src/commands/mod.rs`, `src/commands/hook.rs`
- Modify: `src/main.rs`
- Test: `tests/hook_cmd.rs`

**Interfaces:**
- Consumes: `protocol::*`、`paths::*`
- Produces:
  - `client::request(req: &Request) -> anyhow::Result<Response>`：connect（socket 不存在或拒绝→若 `TFA_NO_SPAWN` 未设则 spawn `current_exe daemon` 后重试一次，间隔 50ms）；读写超时各 100ms
  - `commands::hook::run(agent: &str, event: &str) -> !`：读 stdin 全部 + `TMUX_PANE` env → `Request::Hook` → `client::request` → **无论结果一律 `exit 0`**；stdin 非 JSON 时 payload 用 `Value::Null`；`TMUX_PANE` 缺失也 exit 0（不在 tmux 里就不上报）

- [ ] **Step 1: 写失败的 hook 命令测试**

`tests/hook_cmd.rs`:

```rust
use assert_cmd::Command;
use std::time::{Duration, Instant};

#[test]
fn hook_without_daemon_exits_zero_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", "%3")
        .args(["hook", "claude", "stop"])
        .write_stdin("{}");
    cmd.assert().success();
    assert!(started.elapsed() < Duration::from_millis(500), "hook too slow");
}

#[test]
fn hook_without_tmux_pane_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env_remove("TMUX_PANE")
        .args(["hook", "claude", "stop"])
        .write_stdin("{}");
    cmd.assert().success();
}

#[test]
fn hook_autospawns_daemon_and_event_lands() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    // 无 daemon；hook 应自动拉起并送达
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .env("TMUX_PANE", "%8")
        .args(["hook", "claude", "user-prompt-submit"])
        .write_stdin(r#"{"prompt":"hello"}"#);
    cmd.assert().success();

    // 等 daemon 就绪后查询
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut status = Command::cargo_bin("tfa").unwrap();
    status.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .args(["list"]);
    let out = status.assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""pane_id":"%8""#), "got: {stdout}");
    // 清理：杀掉测试拉起的 daemon
    let _ = std::process::Command::new("pkill")
        .args(["-f", &sock.to_string_lossy()]).status();
}
```

（`tfa list` 此时输出还是 `[]` —— 第三个测试同时驱动 Task 7 之前的最小 list 实现：直接向 daemon 要 snapshot 打印 JSON。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test hook_cmd`
Expected: 第 3 个测试 FAIL（hook 未实现转发、list 不查询 daemon）

- [ ] **Step 3: 实现 client 与 hook**

`src/client.rs`:

```rust
use crate::paths;
use crate::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_millis(100);

fn connect() -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(paths::socket_path())?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn spawn_daemon() {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn request(req: &Request) -> anyhow::Result<Response> {
    let mut stream = match connect() {
        Ok(s) => s,
        Err(_) if std::env::var("TFA_NO_SPAWN").as_deref() != Ok("1") => {
            spawn_daemon();
            std::thread::sleep(Duration::from_millis(50));
            connect()?
        }
        Err(e) => return Err(e.into()),
    };
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    Ok(serde_json::from_str(&resp)?)
}
```

`src/commands/mod.rs`: `pub mod hook; pub mod status;`（status 文件 Task 7 建；本任务先只 `pub mod hook;`）

`src/commands/hook.rs`:

```rust
use crate::client;
use crate::protocol::Request;
use std::io::Read;

/// hook 纪律：任何路径都 exit 0，绝不阻塞 agent。
pub fn run(agent: &str, event: &str) -> ! {
    let Ok(pane) = std::env::var("TMUX_PANE") else { std::process::exit(0) };
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let payload = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
    let _ = client::request(&Request::Hook {
        agent: agent.to_string(),
        event: event.to_string(),
        pane: Some(pane),
        payload,
    });
    std::process::exit(0)
}
```

`src/main.rs`：

```rust
        Command::Hook { agent, event } => commands::hook::run(&agent, &event),
        Command::List => {
            match client::request(&protocol::Request::Snapshot) {
                Ok(protocol::Response::Snapshot { sessions, .. }) => {
                    println!("{}", serde_json::to_string_pretty(&sessions).unwrap_or_default());
                }
                _ => println!("[]"),
            }
        }
```

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（新增 3 个 hook 测试）

```bash
git add src/ tests/
git commit -m "feat: tfa hook forwarder with 100ms discipline and daemon autospawn"
```

---

### Task 7: `tfa status` 渲染与 pane→session 名解析

**Files:**
- Create: `src/commands/status.rs`, `src/render.rs`
- Modify: `src/main.rs`, `src/commands/mod.rs`, `src/daemon/server.rs`
- Test: `src/render.rs` 内联单测

**Interfaces:**
- Consumes: `state::{AgentSession, SessionState}`、`client::request`
- Produces:
  - `render::render_tmux(sessions: &[AgentSession], now_ms: u64) -> String`：无会话→`tfa:idle`；有→`⚡N ⏸N ✓N`（0 计数省略），若有 WaitingInput，追加等最久的 ` ⏸<session_name|pane_id> <mins>m`
  - daemon 侧：`server.rs` 处理 Hook 时若该 pane 的 `session_name` 为空，调 `tmux display-message -p -t <pane> '#{session_name}'` 解析并 `set_session_name`（失败留 None，不报错）
  - `tfa status --format tmux|json`：tmux→渲染行；json→snapshot 原样

- [ ] **Step 1: 写失败的渲染单测**

`src/render.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;
    use crate::state::{AgentSession, SessionState};

    fn sess(pane: &str, name: Option<&str>, state: SessionState, since: u64) -> AgentSession {
        AgentSession {
            pane_id: pane.into(), agent: AgentKind::Claude,
            session_name: name.map(String::from), state,
            state_since_ms: since, current_task: None, cwd: None, last_activity_ms: since,
        }
    }

    #[test]
    fn empty_renders_idle() {
        assert_eq!(render_tmux(&[], 0), "tfa:idle");
    }

    #[test]
    fn counts_and_oldest_waiting_shown() {
        let sessions = vec![
            sess("%1", Some("api"), SessionState::Working, 0),
            sess("%2", Some("web"), SessionState::WaitingInput { reason: "perm".into() }, 60_000),
            sess("%3", None, SessionState::WaitingInput { reason: "input".into() }, 240_000),
            sess("%4", Some("done1"), SessionState::Done, 0),
        ];
        // now = 300s；%2 等了 4min（最久）
        let line = render_tmux(&sessions, 300_000);
        assert_eq!(line, "⚡1 ⏸2 ✓1 ⏸web 4m");
    }

    #[test]
    fn dead_and_zero_counts_omitted() {
        let sessions = vec![sess("%1", None, SessionState::Dead, 0)];
        assert_eq!(render_tmux(&sessions, 0), "tfa:idle");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test render`
Expected: FAIL（编译错误）

- [ ] **Step 3: 实现渲染与命令**

`src/render.rs`:

```rust
use crate::state::{AgentSession, SessionState};

pub fn render_tmux(sessions: &[AgentSession], now_ms: u64) -> String {
    let mut working = 0u32;
    let mut waiting = 0u32;
    let mut done = 0u32;
    let mut oldest_wait: Option<&AgentSession> = None;

    for s in sessions {
        match &s.state {
            SessionState::Working | SessionState::Starting => working += 1,
            SessionState::WaitingInput { .. } => {
                waiting += 1;
                if oldest_wait.map_or(true, |o| s.state_since_ms < o.state_since_ms) {
                    oldest_wait = Some(s);
                }
            }
            SessionState::Done => done += 1,
            SessionState::Dead | SessionState::Stale => {}
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if working > 0 { parts.push(format!("⚡{working}")); }
    if waiting > 0 { parts.push(format!("⏸{waiting}")); }
    if done > 0 { parts.push(format!("✓{done}")); }
    if parts.is_empty() { return "tfa:idle".into(); }

    if let Some(w) = oldest_wait {
        let mins = now_ms.saturating_sub(w.state_since_ms) / 60_000;
        let name = w.session_name.as_deref().unwrap_or(&w.pane_id);
        parts.push(format!("⏸{name} {mins}m"));
    }
    parts.join(" ")
}
```

`src/commands/status.rs`:

```rust
use crate::protocol::{Request, Response};
use crate::{client, render};

pub fn run(format: &str) {
    let resp = client::request(&Request::Snapshot);
    match (format, resp) {
        ("json", Ok(Response::Snapshot { sessions, .. })) => {
            println!("{}", serde_json::to_string_pretty(&sessions).unwrap_or_default());
        }
        ("tmux", Ok(Response::Snapshot { sessions, generated_at_ms })) => {
            println!("{}", render::render_tmux(&sessions, generated_at_ms));
        }
        ("tmux", _) => println!("tfa:off"), // daemon 不可达
        (_, _) => println!("[]"),
    }
}
```

`src/main.rs`：`Command::Status { format } => commands::status::run(&format)`（Task 1 的临时实现删除；`mod render;` 加入）。

`src/daemon/server.rs` 的 Hook 分支，apply 之后补一段（session 名解析，daemon 侧做、非 hook 侧，避免拖慢 hook）：

```rust
                Some(ev) => {
                    let pane = ev.pane_id.clone();
                    let needs_name = {
                        let mut st = store.lock().unwrap();
                        st.apply(ev);
                        st.sessions().iter()
                            .find(|s| s.pane_id == pane)
                            .map_or(false, |s| s.session_name.is_none())
                    };
                    if needs_name {
                        if let Some(name) = resolve_session_name(&pane) {
                            store.lock().unwrap().set_session_name(&pane, name);
                        }
                    }
                    dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
```

同文件新增：

```rust
fn resolve_session_name(pane_id: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(crate::paths::tmux_args())
        .args(["display-message", "-p", "-t", pane_id, "#{session_name}"]);
    let out = cmd.output().ok()?;
    if !out.status.success() { return None; }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}
```

- [ ] **Step 4: 测试通过后提交**

Run: `cargo test`
Expected: PASS（新增 3 个渲染测试；`tests/cli.rs` 的 `tfa:off` 断言现在走真路径仍绿）

```bash
git add src/
git commit -m "feat: tfa status tmux/json rendering with session name resolution"
```

---

### Task 8: Claude Code 插件（hooks 分发）

**Files:**
- Create: `.claude-plugin/plugin.json`, `hooks/hooks.json`, `hook.sh`, `README.md`

**Interfaces:**
- Consumes: `tfa hook claude <event>` CLI（Task 6）
- Produces: 可通过 `/plugin marketplace add <repo路径>` + `/plugin install` 安装的插件；hook.sh 的二进制解析顺序：`$TFA_BIN` → PATH 中的 `tfa` → `~/.cargo/bin/tfa`，找不到则 exit 0

- [ ] **Step 1: 写插件文件**

`.claude-plugin/plugin.json`:

```json
{
  "name": "tfa",
  "description": "tmux for agents — report Claude Code lifecycle events to the tfa daemon",
  "version": "0.1.0",
  "author": { "name": "sbraveyoung" }
}
```

`hook.sh`（布局借鉴 tmux-agent-sidebar 的已验证方案：瘦 wrapper + 静默失败）:

```bash
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
```

`hooks/hooks.json`（事件名与 `EventKind::parse` 一一对应）:

```json
{
  "hooks": {
    "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude session-start" }] }],
    "SessionEnd": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude session-end" }] }],
    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude user-prompt-submit" }] }],
    "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude notification" }] }],
    "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude stop" }] }],
    "PostToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": "bash \"${CLAUDE_PLUGIN_ROOT}/hook.sh\" claude post-tool-use" }] }]
  }
}
```

`README.md`（快速开始）:

```markdown
# tfa — tmux for agents

AI coding agent observability for tmux: who's working, who's waiting
for you, who's done — in your status bar.

## Install (M1)

    cargo install --path .

Claude Code integration (inside claude):

    /plugin marketplace add ~/code/src/github.com/sbraveyoung/tmux_for_agents
    /plugin install tfa

tmux status bar (~/.tmux.conf):

    set -g status-interval 5
    set -g status-right '#(tfa status --format tmux) | %H:%M'

New claude sessions appear automatically. Existing sessions appear
after their next prompt, or restart them with `claude -c`.
```

- [ ] **Step 2: 验证 hook.sh 语法与行为**

```bash
chmod +x hook.sh
bash -n hook.sh && echo "syntax ok"
echo '{}' | TFA_BIN=/nonexistent bash hook.sh claude stop; echo "exit=$?"
```

Expected: `syntax ok`、`exit=0`（TFA_BIN 无效时静默走 PATH 或退出）

- [ ] **Step 3: JSON 校验**

```bash
python3 -c "import json; json.load(open('hooks/hooks.json')); json.load(open('.claude-plugin/plugin.json')); print('valid')"
```

Expected: `valid`

- [ ] **Step 4: Commit**

```bash
git add .claude-plugin/ hooks/ hook.sh README.md
git commit -m "feat: claude code plugin for hook forwarding"
```

---

### Task 9: 端到端冒烟 —— 隔离 tmux + 假 agent

**Files:**
- Create: `tests/e2e.rs`

**Interfaces:**
- Consumes: 全部前序产物（真实二进制、真实 tmux）
- Produces: `cargo test --test e2e`（无 tmux 可执行文件时 skip 不 fail）

- [ ] **Step 1: 写 e2e 测试**

`tests/e2e.rs`:

```rust
use std::process::Command;
use std::time::Duration;

/// 完整链路：隔离 tmux server 里跑假 agent（用 tfa hook 模拟 claude hooks），
/// 断言 tfa status 输出。tmux 不存在时跳过。
#[test]
fn e2e_fake_agent_lifecycle_reflected_in_status() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skip: tmux not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tmux_sock = format!("tfa-e2e-{}", std::process::id());
    let bin = env!("CARGO_BIN_EXE_tfa");
    let envs = [
        ("TFA_SOCKET", dir.path().join("tfa.sock").to_string_lossy().into_owned()),
        ("TFA_STATE_DIR", dir.path().to_string_lossy().into_owned()),
        ("TFA_TMUX_SOCKET", tmux_sock.clone()),
    ];

    let tmux = |args: &[&str]| {
        let mut c = Command::new("tmux");
        c.args(["-L", &tmux_sock]).args(args);
        for (k, v) in &envs { c.env(k, v); }
        c.output().unwrap()
    };

    // 起隔离 tmux + 一个 pane；pane 里模拟 agent 生命周期
    assert!(tmux(&["new-session", "-d", "-s", "proj", "-x", "80", "-y", "24"]).status.success());
    let script = format!(
        "echo '{{\"prompt\":\"build it\"}}' | {bin} hook claude user-prompt-submit; \
         echo '{{\"message\":\"needs permission\"}}' | {bin} hook claude notification"
    );
    assert!(tmux(&["send-keys", "-t", "proj:0.0", &script, "Enter"]).status.success());

    // 轮询 status 直到状态就位（daemon 由 hook 自动拉起）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let expected_frag = "⏸proj";
    loop {
        let mut c = Command::new(bin);
        for (k, v) in &envs { c.env(k, v); }
        c.env("TFA_NO_SPAWN", "1").args(["status", "--format", "tmux"]);
        let out = String::from_utf8(c.output().unwrap().stdout).unwrap();
        if out.contains(expected_frag) { break; }
        assert!(
            std::time::Instant::now() < deadline,
            "status never showed waiting agent; last output: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // 清理：杀 tmux server（daemon 检测到会自杀，但直接 pkill 更快）
    let _ = tmux(&["kill-server"]);
    let _ = Command::new("pkill")
        .args(["-f", &dir.path().join("tfa.sock").to_string_lossy() as &str]).status();
}
```

- [ ] **Step 2: 运行 e2e**

Run: `cargo test --test e2e -- --nocapture`
Expected: PASS。这条测试覆盖：hook 自动拉起 daemon、TMUX_PANE 传递、事件应用、session 名解析（`⏸proj` 证明 display-message 解析成功）、渲染。

- [ ] **Step 3: 全量回归 + Commit**

Run: `cargo test`
Expected: 全部 PASS

```bash
git add tests/e2e.rs
git commit -m "test: end-to-end smoke with isolated tmux server"
```

---

### Task 10: 本机真装验收（用户参与）

**Files:** 无新文件（操作性任务）

- [ ] **Step 1: 安装与接线**

```bash
cargo install --path .
tfa status --format tmux   # 期望: tfa:off 或 tfa:idle（daemon 自动拉起）
```

在 `~/.tmux.conf` 的 status-right 中加入 `#(tfa status --format tmux)`（保留现有 CPU 段），`tmux source-file ~/.tmux.conf`。

在任一 claude 会话里执行 `/plugin marketplace add ~/code/src/github.com/sbraveyoung/tmux_for_agents` 和 `/plugin install tfa`。

- [ ] **Step 2: 真实验收（需用户确认）**

新开一个 claude 会话发条消息，观察状态栏出现 `⚡1`；等它需要权限确认时变 `⏸1 ⏸<session> 0m`；回答后完成变 `✓1`。

**此任务必须由用户亲自验收通过后才算完成（用户工作纪律：agent 产出逐个独立验收）。**

---

## Self-Review 记录

- **Spec 覆盖**：M1 范围五要素（daemon 生命周期=Task 4/5、hook 纪律=Task 6、claude 插件=Task 8、状态机六态=Task 3、status 输出=Task 7）全部有任务对应；spec 的 subscribe op 明确延后（见 Architecture 段），`tfa notify`/scanner/资源指标属 M2/M3 不在本计划。
- **占位符扫描**：Task 2 曾有「Snapshot 暂用 Value」的过渡，已在任务内写明具体做法与 Task 3 的回改点，非悬空 TODO。
- **类型一致性**：`AgentSession` 字段在 Task 3 定义、Task 5 快照 fixture、Task 7 渲染测试三处对齐（含 `#[serde(flatten)]` 的 JSON 形状）；`EventKind::parse` 的事件字符串与 Task 8 hooks.json 的命令参数一一对应；`tfa:off`（daemon 不可达）与 `tfa:idle`(无会话)语义在 Task 1/7 区分明确。
