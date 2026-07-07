# tfa M2 资源指标 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 补上双通道的扫描通道（存量会话建档、死亡纠偏）+ 资源指标（claude 的 context%/model/token，codex 的 model/token），指标经 `tfa list` / `tfa status --format json` 对外可见。

**Architecture:** daemon 内新增 scanner 线程（周期 reconcile）：tmux list-panes + ps 进程树建立 pane↔agent 绑定；hook 已知会话用 payload 里的 `transcript_path` 精确绑定 claude 会话文件，扫描发现的会话用 cwd 编码规则兜底发现；codex 走 `~/.codex/state_5.sqlite` 的 `threads` 表只读查询。状态与指标全部汇入现有 StateStore，快照向后兼容（全部新字段 `#[serde(default)]`）。

**Tech Stack:** Rust 2021、std 线程（无 async runtime）、serde/serde_json、rusqlite（bundled，本里程碑唯一新依赖）。

## Global Constraints

- edition 2021；不引入 async runtime；并发一律 std::thread + Arc<Mutex>。
- 新依赖仅 `rusqlite = { version = "0.37", features = ["bundled"] }`；dev-deps 不变。
- hook 纪律神圣不可侵犯：scanner 只活在 daemon 里，`tfa hook` 路径一行不改动其时序；IO_TIMEOUT=100ms、SPAWN_RETRY_DELAY=50ms 常量测试保持通过。
- 时间戳一律 epoch 毫秒 u64，字段名 `_ms` 后缀。
- 快照/wire 只做加法：M1 的 snapshot.json 必须能被 M2 反序列化（有测试钉死）。
- 环境变量开关（测试隔离）：已有 TFA_SOCKET / TFA_STATE_DIR / TFA_TMUX_SOCKET / TFA_NO_SPAWN / TFA_SKIP_TMUX_CHECK / TFA_TMUX_CHECK_INTERVAL_MS；本里程碑新增 **TFA_SCAN_INTERVAL_MS**（默认 15000）、**TFA_CLAUDE_PROJECTS_DIR**（默认 `$HOME/.claude/projects`）、**TFA_CODEX_DB**（默认 `$HOME/.codex/state_5.sqlite`）、**TFA_NO_SCAN**（=1 时 daemon 不起 scanner，供 M1 行为的既有测试隔离用）。
- scanner 调 tmux 必须带 `paths::tmux_args()`（`-L` 隔离）。
- 坏行/半写/文件缺失/表为空：跳过或返回 None，绝不 panic、绝不炸整轮扫描。
- 本机实测事实（设计依据，fixture 必须照抄这些形状）：
  - claude 会话 JSONL 在 `~/.claude/projects/<cwd 编码>/<session-id>.jsonl`，cwd 编码 = 把 `[^A-Za-z0-9]` 全部替换为 `-`（实测 `/Users/sbraveyoung/code/.../tmux_for_agents/.claude/worktrees/calm-fox-mlzj` → `-Users-sbraveyoung-code-...-tmux-for-agents--claude-worktrees-calm-fox-mlzj`）。
  - assistant 行形如 `{"type":"assistant","gitBranch":"...","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"cache_creation_input_tokens":705,"cache_read_input_tokens":982162,"output_tokens":1045,...}},...}`；存在 `"model":"<synthetic>"` 的占位行（usage 全 0，必须过滤）；首行可能是 `{"type":"last-prompt",...}` 等非消息行。
  - claude hook stdin JSON 自带顶层 `session_id`、`transcript_path`、`cwd`（M1 已原样转发为 payload）。
  - claude 进程 argv0 是 `/Users/<u>/.local/share/claude/versions/<x.y.z>`（不含字面 "claude" 命令名，tmux `pane_current_command` 显示为版本号）；subagent 进程带 `--agent-id` 参数；另有 `claude daemon run` 常驻进程必须排除。
  - codex `threads` 表关键列：`id TEXT PK, rollout_path TEXT, cwd TEXT, title TEXT, tokens_used INTEGER, model TEXT, updated_at_ms INTEGER, archived INTEGER`；本机该表当前 0 行（reader 必须优雅处理空表/缺库）。

## 状态机新规则（Task 1 实现，后续任务依赖）

| 场景 | 规则 |
|---|---|
| 同 pane 收到不同 AgentKind 的 hook 事件（pane 复用漂移） | 整条 entry 重建（新身份，旧指标清空） |
| 已知 pane 收到 SessionStart（新会话复用 pane） | 保留 pane 绑定，重置 current_task/cwd/指标/transcript/session_id，state→Starting |
| 扫描发现无档 pane | 建档 source=Scan，状态 Working（进程在跑就当在干活；下一个 hook 事件会纠正） |
| entry 的 pane 从 tmux 消失，或 pane 在但 agent 进程没了 | state→Dead（M1 遗留的「kill 窗口留幽灵 working」在此根治） |
| state=Working 且 now-last_activity_ms > 300_000 且进程仍在 | state→Stale（心跳每 2s 一次，5 分钟无活动即矛盾） |
| hook 建档的 entry 被扫描确认 | source Hook→Both，补 pid/session_name |

---

### Task 1: 数据模型扩展与状态机新规则（state.rs）

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs`（模块内单测，沿用 M1 风格）

**Interfaces:**
- Consumes: `crate::event::{AgentEvent, AgentKind, EventKind}`（M1 现有；本任务同时给 `AgentEvent` 加两个字段，见 Step 3）
- Produces（后续所有任务依赖的精确形状）:
  - `enum Source { Hook, Scan, Both }`（serde snake_case，`Default = Hook`）
  - `struct ContextUsage { used_tokens: u64, max_tokens: Option<u64>, percent: Option<u8> }`
  - `struct TokenTotals { input: u64, output: u64, cache_read: u64, cache_creation: u64, total: u64 }`
  - `struct SessionMetrics { model: Option<String>, context: Option<ContextUsage>, tokens: Option<TokenTotals>, git_branch: Option<String> }`
  - `AgentSession` 新字段（全部 `#[serde(default)]`）: `source: Source, pid: Option<u32>, model: Option<String>, context: Option<ContextUsage>, tokens: Option<TokenTotals>, git_branch: Option<String>, transcript_path: Option<String>, agent_session_id: Option<String>`
  - `StateStore` 新方法:
    - `pub fn upsert_scanned(&mut self, pane_id: &str, agent: AgentKind, pid: u32, cwd: &str, session_name: &str, now_ms: u64)`
    - `pub fn reconcile_liveness(&mut self, live: &std::collections::BTreeMap<String, Option<u32>>, now_ms: u64)`（key=pane_id；`Some(pid)`=agent 进程在；`None`=pane 在但无 agent 进程；**不在 map 里=pane 已消失**）
    - `pub fn stale_sweep(&mut self, now_ms: u64)`
    - `pub fn set_metrics(&mut self, pane_id: &str, m: SessionMetrics)`
    - `pub fn set_transcript(&mut self, pane_id: &str, path: String)`
    - `pub fn panes_needing_name(&self) -> Vec<String>`（session_name 为 None 的 pane 列表）
  - `pub const STALE_AFTER_MS: u64 = 300_000;`

- [ ] **Step 1: 写失败测试（新类型序列化形状 + M1 快照向后兼容 + 状态机新规则）**

在 `src/state.rs` 的 `mod tests` 追加：

```rust
#[test]
fn m1_snapshot_still_loads() {
    // M1 生产快照的真实形状（无 M2 字段）——升级后必须能加载
    let m1_json = r#"{"sessions":{"%1":{"pane_id":"%1","agent":"claude","session_name":"api","state":"working","state_since_ms":100,"current_task":"fix","cwd":"/tmp/p","last_activity_ms":200}}}"#;
    let store = StateStore::from_json(m1_json).unwrap();
    let s = &store.sessions()[0];
    assert!(matches!(s.source, Source::Hook)); // default
    assert!(s.model.is_none() && s.context.is_none() && s.tokens.is_none());
}

#[test]
fn metrics_serialize_shape() {
    let m = SessionMetrics {
        model: Some("claude-fable-5".into()),
        context: Some(ContextUsage { used_tokens: 982869, max_tokens: Some(1_000_000), percent: Some(98) }),
        tokens: Some(TokenTotals { input: 2, output: 1045, cache_read: 982162, cache_creation: 705, total: 983914 }),
        git_branch: Some("main".into()),
    };
    let mut st = StateStore::new();
    st.upsert_scanned("%9", AgentKind::Claude, 4242, "/tmp/p", "api", 1000);
    st.set_metrics("%9", m);
    let json = serde_json::to_string(&st.sessions()[0]).unwrap();
    assert!(json.contains(r#""source":"scan""#), "json: {json}");
    assert!(json.contains(r#""used_tokens":982869"#));
    assert!(json.contains(r#""percent":98"#));
    assert!(json.contains(r#""pid":4242"#));
}

#[test]
fn scanned_pane_starts_working_and_hook_upgrades_source() {
    let mut st = StateStore::new();
    st.upsert_scanned("%1", AgentKind::Claude, 42, "/tmp/p", "api", 1000);
    let s = &st.sessions()[0];
    assert!(matches!(s.state, SessionState::Working));
    assert!(matches!(s.source, Source::Scan));
    assert_eq!(s.session_name.as_deref(), Some("api"));
    // 再收 hook 事件 → Both，状态被事件接管
    st.apply(ev(EventKind::Stop, 2000));
    let s = &st.sessions()[0];
    assert!(matches!(s.source, Source::Both));
    assert!(matches!(s.state, SessionState::Done));
}

#[test]
fn upsert_scanned_on_existing_entry_does_not_reset_state() {
    let mut st = StateStore::new();
    st.apply(ev(EventKind::Notification, 1000)); // WaitingInput via hook
    st.upsert_scanned("%1", AgentKind::Claude, 42, "/tmp/p", "api", 2000);
    let s = &st.sessions()[0];
    assert!(matches!(s.state, SessionState::WaitingInput { .. }), "扫描确认不得覆盖事件状态");
    assert!(matches!(s.source, Source::Both));
    assert_eq!(s.pid, Some(42));
}

#[test]
fn agent_kind_drift_rebuilds_entry() {
    let mut st = StateStore::new();
    let mut e = ev(EventKind::UserPromptSubmit, 1000);
    e.prompt = Some("old claude task".into());
    st.apply(e);
    // 同 pane 换了 agent（pane 复用）
    let mut e2 = ev(EventKind::SessionStart, 2000);
    e2.agent = AgentKind::Codex;
    st.apply(e2);
    let s = &st.sessions()[0];
    assert!(matches!(s.agent, AgentKind::Codex));
    assert!(s.current_task.is_none(), "旧任务必须清空");
}

#[test]
fn session_start_on_known_pane_resets_metadata() {
    let mut st = StateStore::new();
    let mut e = ev(EventKind::UserPromptSubmit, 1000);
    e.prompt = Some("task A".into());
    st.apply(e);
    st.set_metrics("%1", SessionMetrics {
        model: Some("claude-fable-5".into()), context: None, tokens: None, git_branch: None,
    });
    st.apply(ev(EventKind::SessionStart, 2000));
    let s = &st.sessions()[0];
    assert!(matches!(s.state, SessionState::Starting));
    assert!(s.current_task.is_none() && s.model.is_none(), "新会话不得继承旧指标");
}

#[test]
fn reconcile_liveness_marks_dead() {
    let mut st = StateStore::new();
    st.apply(ev(EventKind::UserPromptSubmit, 1000));          // %1 working
    st.upsert_scanned("%2", AgentKind::Claude, 7, "/p", "s", 1000);
    let mut live = std::collections::BTreeMap::new();
    live.insert("%2".to_string(), None::<u32>);               // pane 在、进程没了
    // %1 不在 map → pane 已消失
    st.reconcile_liveness(&live, 5000);
    let sessions = st.sessions();
    assert!(sessions.iter().all(|s| matches!(s.state, SessionState::Dead)));
    assert!(sessions.iter().all(|s| s.state_since_ms == 5000));
}

#[test]
fn reconcile_liveness_leaves_live_and_dead_alone() {
    let mut st = StateStore::new();
    st.apply(ev(EventKind::SessionEnd, 1000)); // 已 Dead
    let mut live = std::collections::BTreeMap::new();
    live.insert("%1".to_string(), Some(9u32));
    st.reconcile_liveness(&live, 5000);
    // 已 Dead 的不被复活也不重置 state_since（否则 prune 永不触发）
    let s = &st.sessions()[0];
    assert!(matches!(s.state, SessionState::Dead));
    assert_eq!(s.state_since_ms, 1000);
}

#[test]
fn stale_sweep_flags_quiet_working_sessions() {
    let mut st = StateStore::new();
    st.apply(ev(EventKind::UserPromptSubmit, 1000)); // last_activity=1000
    st.stale_sweep(1000 + STALE_AFTER_MS + 1);
    assert!(matches!(st.sessions()[0].state, SessionState::Stale));
    // Stale 后新事件照常接管
    st.apply(ev(EventKind::Stop, 999_999));
    assert!(matches!(st.sessions()[0].state, SessionState::Done));
}

#[test]
fn hook_event_stores_transcript_and_session_id() {
    let mut st = StateStore::new();
    let mut e = ev(EventKind::SessionStart, 1000);
    e.transcript_path = Some("/tmp/t.jsonl".into());
    e.session_id = Some("abc-123".into());
    st.apply(e);
    let s = &st.sessions()[0];
    assert_eq!(s.transcript_path.as_deref(), Some("/tmp/t.jsonl"));
    assert_eq!(s.agent_session_id.as_deref(), Some("abc-123"));
}

#[test]
fn panes_needing_name_lists_unnamed_only() {
    let mut st = StateStore::new();
    st.apply(ev(EventKind::SessionStart, 0));                       // %1 无名
    st.upsert_scanned("%2", AgentKind::Claude, 7, "/p", "named", 0); // %2 有名
    assert_eq!(st.panes_needing_name(), vec!["%1".to_string()]);
}
```

同时把 tests 顶部的 `fn ev` 辅助函数改为带新字段的默认值：

```rust
fn ev(kind: EventKind, at_ms: u64) -> AgentEvent {
    AgentEvent {
        agent: AgentKind::Claude, pane_id: "%1".into(), kind,
        reason: None, prompt: None, cwd: None, at_ms,
        transcript_path: None, session_id: None,
    }
}
```

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo test state 2>&1 | tail -20`
Expected: 编译错误（`Source`/`SessionMetrics`/`upsert_scanned` 等不存在，`AgentEvent` 无 `transcript_path` 字段）。把输出存档进报告（RED 证据）。

- [ ] **Step 3: 实现**

`src/event.rs` 的 `AgentEvent` 加字段并在 `from_hook` 提取（claude hook stdin 顶层就有这两个 key）：

```rust
pub struct AgentEvent {
    pub agent: AgentKind,
    pub pane_id: String,
    pub kind: EventKind,
    pub reason: Option<String>,
    pub prompt: Option<String>,
    pub cwd: Option<String>,
    pub at_ms: u64,
    pub transcript_path: Option<String>,
    pub session_id: Option<String>,
}
// from_hook 里：
            transcript_path: str_field(payload, "transcript_path"),
            session_id: str_field(payload, "session_id"),
```

`src/state.rs`：

```rust
pub const STALE_AFTER_MS: u64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Hook,
    Scan,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub max_tokens: Option<u64>,
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMetrics {
    pub model: Option<String>,
    pub context: Option<ContextUsage>,
    pub tokens: Option<TokenTotals>,
    pub git_branch: Option<String>,
}
```

`AgentSession` 追加（放在 `last_activity_ms` 之后）：

```rust
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub context: Option<ContextUsage>,
    #[serde(default)]
    pub tokens: Option<TokenTotals>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
}
```

`StateStore` 实现（`apply` 改造 + 新方法；`fresh_session` 是私有辅助，统一「新建 entry」逻辑避免三处复制）：

```rust
fn fresh_session(pane_id: &str, agent: AgentKind, source: Source, at_ms: u64) -> AgentSession {
    AgentSession {
        pane_id: pane_id.to_string(),
        agent,
        session_name: None,
        state: if matches!(source, Source::Scan) { SessionState::Working } else { SessionState::Starting },
        state_since_ms: at_ms,
        current_task: None,
        cwd: None,
        last_activity_ms: at_ms,
        source,
        pid: None,
        model: None,
        context: None,
        tokens: None,
        git_branch: None,
        transcript_path: None,
        agent_session_id: None,
    }
}

impl StateStore {
    pub fn apply(&mut self, ev: AgentEvent) {
        // pane 复用漂移：同 pane 不同 agent → 整条重建
        if let Some(existing) = self.sessions.get(&ev.pane_id) {
            if existing.agent != ev.agent {
                self.sessions.insert(ev.pane_id.clone(), fresh_session(&ev.pane_id, ev.agent.clone(), Source::Hook, ev.at_ms));
            }
        }
        // SessionStart 元数据重置：已知 pane 开新会话，不继承旧任务/指标
        if matches!(ev.kind, EventKind::SessionStart) && self.sessions.contains_key(&ev.pane_id) {
            let name = self.sessions.get(&ev.pane_id).and_then(|s| s.session_name.clone());
            let mut fresh = fresh_session(&ev.pane_id, ev.agent.clone(), Source::Hook, ev.at_ms);
            fresh.session_name = name; // tmux session 名跟 pane 走，保留
            self.sessions.insert(ev.pane_id.clone(), fresh);
        }
        let entry = self.sessions.entry(ev.pane_id.clone()).or_insert_with(|| {
            fresh_session(&ev.pane_id, ev.agent.clone(), Source::Hook, ev.at_ms)
        });
        entry.last_activity_ms = ev.at_ms;
        if matches!(entry.source, Source::Scan) { entry.source = Source::Both; }
        if let Some(cwd) = ev.cwd { entry.cwd = Some(cwd); }
        if let Some(t) = ev.transcript_path { entry.transcript_path = Some(t); }
        if let Some(id) = ev.session_id { entry.agent_session_id = Some(id); }

        let next = match ev.kind { /* M1 原样保留 */ };
        if let Some(state) = next {
            entry.state = state;
            entry.state_since_ms = ev.at_ms;
        }
    }

    pub fn upsert_scanned(&mut self, pane_id: &str, agent: AgentKind, pid: u32, cwd: &str, session_name: &str, now_ms: u64) {
        // 漂移检查同 apply
        if let Some(existing) = self.sessions.get(pane_id) {
            if existing.agent != agent {
                self.sessions.insert(pane_id.to_string(), fresh_session(pane_id, agent.clone(), Source::Scan, now_ms));
            }
        }
        let entry = self.sessions.entry(pane_id.to_string())
            .or_insert_with(|| fresh_session(pane_id, agent.clone(), Source::Scan, now_ms));
        entry.pid = Some(pid);
        if entry.cwd.is_none() { entry.cwd = Some(cwd.to_string()); }
        if entry.session_name.is_none() { entry.session_name = Some(session_name.to_string()); }
        if matches!(entry.source, Source::Hook) { entry.source = Source::Both; }
    }

    pub fn reconcile_liveness(&mut self, live: &std::collections::BTreeMap<String, Option<u32>>, now_ms: u64) {
        for s in self.sessions.values_mut() {
            if matches!(s.state, SessionState::Dead) { continue; }
            match live.get(&s.pane_id) {
                Some(Some(pid)) => { s.pid = Some(*pid); }
                Some(None) | None => {
                    s.state = SessionState::Dead;
                    s.state_since_ms = now_ms;
                }
            }
        }
    }

    pub fn stale_sweep(&mut self, now_ms: u64) {
        for s in self.sessions.values_mut() {
            if matches!(s.state, SessionState::Working)
                && now_ms.saturating_sub(s.last_activity_ms) > STALE_AFTER_MS
            {
                s.state = SessionState::Stale;
                s.state_since_ms = now_ms;
            }
        }
    }

    pub fn set_metrics(&mut self, pane_id: &str, m: SessionMetrics) {
        if let Some(s) = self.sessions.get_mut(pane_id) {
            if m.model.is_some() { s.model = m.model; }
            if m.context.is_some() { s.context = m.context; }
            if m.tokens.is_some() { s.tokens = m.tokens; }
            if m.git_branch.is_some() { s.git_branch = m.git_branch; }
        }
    }

    pub fn set_transcript(&mut self, pane_id: &str, path: String) {
        if let Some(s) = self.sessions.get_mut(pane_id) {
            if s.transcript_path.is_none() { s.transcript_path = Some(path); }
        }
    }

    pub fn panes_needing_name(&self) -> Vec<String> {
        self.sessions.values()
            .filter(|s| s.session_name.is_none())
            .map(|s| s.pane_id.clone())
            .collect()
    }
}
```

注意：render.rs 的测试辅助 `sess()` 和 e2e 涉及 `AgentSession` 字面构造的地方需要补新字段默认值（`..` 展开不可用因为没有 Default —— 给 `AgentSession` 直接 `#[derive(Default)]` 不行（AgentKind 无 Default），测试辅助里手写补齐即可）。

- [ ] **Step 4: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: 全部通过（M1 的 34 个 + 新增 10 个），无 warning。

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/event.rs src/render.rs
git commit -m "feat(m2): extend data model with metrics fields and scanner state rules"
```

---

### Task 2: claude 会话 JSONL 增量解析器（sources/claude_jsonl.rs）

**Files:**
- Create: `src/sources/mod.rs`
- Create: `src/sources/claude_jsonl.rs`
- Modify: `src/main.rs`（`mod sources;`）

**Interfaces:**
- Consumes: `crate::state::{ContextUsage, SessionMetrics, TokenTotals}`（Task 1）
- Produces:
  - `pub struct TranscriptCursor { pub offset: u64 }`（`Default` = 0）
  - `pub fn read_update(path: &std::path::Path, cursor: &mut TranscriptCursor) -> Option<SessionMetrics>`
  - `pub fn context_window(model: &str) -> Option<u64>`
  - `pub fn encode_cwd(cwd: &str) -> String`
  - `pub fn discover_transcript(projects_dir: &std::path::Path, cwd: &str) -> Option<std::path::PathBuf>`
  - `pub const TAIL_CAP: u64 = 262_144;`

**语义约定：**
- `read_update` 从 `cursor.offset` 读到文件尾；首次读大文件（offset=0 且 len>TAIL_CAP）跳到 `len-TAIL_CAP` 并丢弃首个不完整行；offset 只推进到**最后一个完整行**（`\n` 结尾）之后，半写行留给下一轮；文件缩短（rotate/truncate）→ offset 归零重读。
- 取窗口内**最后一条**有 `message.usage` 且 `message.model != "<synthetic>"` 的 assistant 行产出指标；窗口内没有 → 返回 None（保留旧指标，set_metrics 的 Some-覆盖语义配合）。
- `used_tokens = input + cache_read + cache_creation`（当前上下文占用）；`total = input + output + cache_read + cache_creation`。
- `context_window`: `claude-fable` 前缀 → 1_000_000；其余 `claude-` 前缀 → 200_000；其它 → None（percent 为 None，UI 只展示绝对量）。
- `encode_cwd`: `[^A-Za-z0-9]` → `-`。
- `discover_transcript`: `projects_dir/<encode_cwd(cwd)>/` 下 mtime 最新的 `*.jsonl`。

- [ ] **Step 1: 写失败测试**

`src/sources/claude_jsonl.rs` 尾部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // 形状照抄本机真实行（数值取自 2026-07-07 实测）
    const REAL_ASSISTANT: &str = r#"{"type":"assistant","gitBranch":"main","cwd":"/tmp/p","sessionId":"a5f1915b","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"cache_creation_input_tokens":705,"cache_read_input_tokens":982162,"output_tokens":1045,"service_tier":"standard"}},"uuid":"u1"}"#;
    const SYNTHETIC: &str = r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"uuid":"u2"}"#;
    const HEADER: &str = r#"{"type":"last-prompt","leafUuid":"x","sessionId":"a5f1915b"}"#;

    fn write_lines(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines { writeln!(f, "{l}").unwrap(); }
        f.flush().unwrap();
        f
    }

    #[test]
    fn parses_last_real_assistant_and_skips_noise() {
        let f = write_lines(&[HEADER, "not json at all {", REAL_ASSISTANT, SYNTHETIC]);
        let mut cur = TranscriptCursor::default();
        let m = read_update(f.path(), &mut cur).unwrap();
        assert_eq!(m.model.as_deref(), Some("claude-fable-5"));
        let t = m.tokens.unwrap();
        assert_eq!(t.cache_read, 982162);
        assert_eq!(t.total, 2 + 1045 + 982162 + 705);
        let c = m.context.unwrap();
        assert_eq!(c.used_tokens, 2 + 982162 + 705);
        assert_eq!(c.max_tokens, Some(1_000_000));
        assert_eq!(c.percent, Some(98));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn incremental_read_only_sees_new_lines_and_holds_partial() {
        let f = write_lines(&[REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        assert!(read_update(f.path(), &mut cur).is_some());
        let after_first = cur.offset;
        // 无新内容 → None，offset 不动
        assert!(read_update(f.path(), &mut cur).is_none());
        assert_eq!(cur.offset, after_first);
        // 追加半行（无 \n）→ None，offset 不越过半行
        let mut fh = std::fs::OpenOptions::new().append(true).open(f.path()).unwrap();
        write!(fh, "{}", &REAL_ASSISTANT[..40]).unwrap();
        fh.flush().unwrap();
        assert!(read_update(f.path(), &mut cur).is_none());
        assert_eq!(cur.offset, after_first);
        // 补全该行 + 换行 → 解析成功
        writeln!(fh, "{}", &REAL_ASSISTANT[40..]).unwrap();
        fh.flush().unwrap();
        assert!(read_update(f.path(), &mut cur).is_some());
    }

    #[test]
    fn truncated_file_resets_offset() {
        let f = write_lines(&[REAL_ASSISTANT, REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        std::fs::write(f.path(), format!("{REAL_ASSISTANT}\n")).unwrap(); // 变短
        let m = read_update(f.path(), &mut cur);
        assert!(m.is_some(), "缩短的文件应从头重读");
    }

    #[test]
    fn missing_file_returns_none() {
        let mut cur = TranscriptCursor::default();
        assert!(read_update(std::path::Path::new("/nonexistent/x.jsonl"), &mut cur).is_none());
    }

    #[test]
    fn context_window_table() {
        assert_eq!(context_window("claude-fable-5"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-8"), Some(200_000));
        assert_eq!(context_window("gpt-x"), None);
    }

    #[test]
    fn encode_cwd_matches_observed_layout() {
        assert_eq!(
            encode_cwd("/Users/u/code/tmux_for_agents/.claude/worktrees/calm-fox-mlzj"),
            "-Users-u-code-tmux-for-agents--claude-worktrees-calm-fox-mlzj"
        );
    }

    #[test]
    fn discover_transcript_picks_newest_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(encode_cwd("/tmp/proj"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("old.jsonl"), "x").unwrap();
        let old_t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::File::open(proj.join("old.jsonl")).unwrap();
        f.set_modified(old_t).unwrap();
        std::fs::write(proj.join("new.jsonl"), "x").unwrap();
        std::fs::write(proj.join("ignore.txt"), "x").unwrap();
        let found = discover_transcript(dir.path(), "/tmp/proj").unwrap();
        assert!(found.ends_with("new.jsonl"));
        assert!(discover_transcript(dir.path(), "/no/such/cwd").is_none());
    }
}
```

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo test claude_jsonl 2>&1 | tail -10`
Expected: 编译错误（模块不存在）。存档 RED 证据。

- [ ] **Step 3: 实现**

`src/sources/mod.rs`：

```rust
pub mod claude_jsonl;
```

`src/sources/claude_jsonl.rs`：

```rust
use crate::state::{ContextUsage, SessionMetrics, TokenTotals};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const TAIL_CAP: u64 = 262_144;

#[derive(Debug, Default, Clone)]
pub struct TranscriptCursor {
    pub offset: u64,
}

pub fn context_window(model: &str) -> Option<u64> {
    if model.starts_with("claude-fable") {
        Some(1_000_000)
    } else if model.starts_with("claude-") {
        Some(200_000)
    } else {
        None
    }
}

pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub fn discover_transcript(projects_dir: &Path, cwd: &str) -> Option<PathBuf> {
    let dir = projects_dir.join(encode_cwd(cwd));
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

pub fn read_update(path: &Path, cursor: &mut TranscriptCursor) -> Option<SessionMetrics> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < cursor.offset { cursor.offset = 0; } // truncate/rotate → 重读
    if cursor.offset == len { return None; }
    let mut skip_first_partial = false;
    if cursor.offset == 0 && len > TAIL_CAP {
        cursor.offset = len - TAIL_CAP;
        skip_first_partial = true;
    }
    file.seek(SeekFrom::Start(cursor.offset)).ok()?;
    let mut buf = Vec::with_capacity((len - cursor.offset) as usize);
    file.take(len - cursor.offset).read_to_end(&mut buf).ok()?;

    // 只处理到最后一个完整行；半写行留待下一轮
    let complete_end = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1)?;
    let window = &buf[..complete_end];
    cursor.offset += complete_end as u64;

    let text = String::from_utf8_lossy(window);
    let mut lines = text.lines();
    if skip_first_partial { lines.next(); }

    let mut latest: Option<SessionMetrics> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue }; // 坏行跳过
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") { continue; }
        let Some(msg) = v.get("message") else { continue };
        let Some(model) = msg.get("model").and_then(|m| m.as_str()) else { continue };
        if model == "<synthetic>" { continue; }
        let Some(usage) = msg.get("usage") else { continue };
        let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let tokens = TokenTotals {
            input: g("input_tokens"),
            output: g("output_tokens"),
            cache_read: g("cache_read_input_tokens"),
            cache_creation: g("cache_creation_input_tokens"),
            total: g("input_tokens") + g("output_tokens")
                + g("cache_read_input_tokens") + g("cache_creation_input_tokens"),
        };
        let used = tokens.input + tokens.cache_read + tokens.cache_creation;
        let max = context_window(model);
        let context = ContextUsage {
            used_tokens: used,
            max_tokens: max,
            percent: max.map(|m| ((used.saturating_mul(100)) / m.max(1)).min(100) as u8),
        };
        latest = Some(SessionMetrics {
            model: Some(model.to_string()),
            context: Some(context),
            tokens: Some(tokens),
            git_branch: v.get("gitBranch").and_then(|b| b.as_str()).map(String::from),
        });
    }
    latest
}
```

`src/main.rs` 加 `mod sources;`（挂在现有 mod 列表里；daemon 侧 Task 4 才消费，暂时按 M1 惯例 `#[allow(dead_code)]` 挂载：`#[allow(dead_code)] // consumed by scanner from Task 4 onward` `mod sources;`）。

- [ ] **Step 4: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: 全绿无 warning。

- [ ] **Step 5: Commit**

```bash
git add src/sources/ src/main.rs
git commit -m "feat(m2): incremental claude transcript parser with context accounting"
```

---

### Task 3: pane/进程发现与匹配（scanner/procs.rs）

**Files:**
- Create: `src/scanner/mod.rs`（本任务只放 `pub mod procs;`，reconcile 循环 Task 4 再填）
- Create: `src/scanner/procs.rs`
- Modify: `src/main.rs`（`mod scanner;`，同样先 `#[allow(dead_code)]`）

**Interfaces:**
- Consumes: `crate::event::AgentKind`、`crate::paths::tmux_args`
- Produces:
  - `pub struct PaneInfo { pub pane_id: String, pub pane_pid: u32, pub cwd: String, pub session_name: String }`
  - `pub struct ProcEntry { pub pid: u32, pub ppid: u32, pub args: String }`
  - `pub fn parse_panes(out: &str) -> Vec<PaneInfo>`（tab 分隔 4 列）
  - `pub fn parse_ps(out: &str) -> Vec<ProcEntry>`
  - `pub fn classify(args: &str) -> Option<AgentKind>`
  - `pub fn find_agent(pane_pid: u32, procs: &[ProcEntry]) -> Option<(AgentKind, u32)>`（BFS，取最浅匹配）
  - `pub fn list_panes() -> Vec<PaneInfo>`（`tmux <tmux_args> list-panes -a -F '#{pane_id}\t#{pane_pid}\t#{pane_current_path}\t#{session_name}'`，失败→空）
  - `pub fn list_procs() -> Vec<ProcEntry>`（`ps -axo pid=,ppid=,args=`，失败→空）

**classify 规则（本机实测形状）：**
- Claude：argv0（args 第一个空格前的 token）路径包含 `/claude/versions/`，或其 basename == `claude`；但 args 包含 ` daemon ` 或以 ` daemon` 结尾的排除（`claude daemon run` 常驻进程不是 pane agent）。
- Codex：argv0 basename == `codex` 或以 `codex-` 开头（cask 二进制名 `codex-x86_64-apple-darwin`）。
- 其他（含 `tmux-agent-side`、普通 zsh）→ None。Custom agent 的进程识别 adapter 留给后续里程碑。

- [ ] **Step 1: 写失败测试**

`src/scanner/procs.rs` 尾部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;

    #[test]
    fn parse_panes_splits_tab_fields() {
        let out = "%1\t123\t/Users/u/proj\tcompany\n%22\t456\t/tmp/x y z\tLLM\n";
        let panes = parse_panes(out);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[0].pane_pid, 123);
        assert_eq!(panes[1].cwd, "/tmp/x y z");
        assert_eq!(panes[1].session_name, "LLM");
        assert!(parse_panes("garbage without tabs\n").is_empty());
    }

    #[test]
    fn classify_matches_real_argv_shapes() {
        // 本机实测：claude 的 argv0 是版本路径，不含字面 "claude" 命令名
        assert_eq!(classify("/Users/u/.local/share/claude/versions/2.1.202"), Some(AgentKind::Claude));
        assert_eq!(classify("/Users/u/.local/share/claude/versions/2.1.202 --agent-id x@y --model claude-opus-4-8"), Some(AgentKind::Claude));
        assert_eq!(classify("/Users/u/.local/bin/claude"), Some(AgentKind::Claude));
        assert_eq!(classify("/opt/homebrew/bin/codex"), Some(AgentKind::Codex));
        assert_eq!(classify("/usr/local/Caskroom/codex/0.142.4/codex-x86_64-apple-darwin"), Some(AgentKind::Codex));
        // 排除项
        assert_eq!(classify("/Users/u/.local/bin/claude daemon run --origin transient"), None);
        assert_eq!(classify("tmux-agent-sidebar --collector"), None);
        assert_eq!(classify("-zsh"), None);
        assert_eq!(classify("/bin/zsh -il"), None);
    }

    #[test]
    fn find_agent_walks_tree_and_prefers_shallowest() {
        let procs = vec![
            ProcEntry { pid: 100, ppid: 1, args: "/bin/zsh -il".into() },              // pane shell
            ProcEntry { pid: 200, ppid: 100, args: "/U/.local/share/claude/versions/2.1.202".into() },
            ProcEntry { pid: 300, ppid: 200, args: "/U/.local/share/claude/versions/2.1.202 --agent-id sub@x".into() },
            ProcEntry { pid: 999, ppid: 1, args: "/opt/homebrew/bin/codex".into() },   // 别的 pane 的
        ];
        let (kind, pid) = find_agent(100, &procs).unwrap();
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(pid, 200, "取最浅的主进程而非 subagent 子进程");
        assert!(find_agent(999, &procs).is_some(), "pane 进程自身就是 agent 也要匹配");
        assert!(find_agent(4242, &procs).is_none());
    }
}
```

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo test procs 2>&1 | tail -10`
Expected: 编译错误（模块不存在）。存档 RED 证据。

- [ ] **Step 3: 实现**

`src/scanner/mod.rs`：

```rust
pub mod procs;
```

`src/scanner/procs.rs`：

```rust
use crate::event::AgentKind;
use crate::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub pane_pid: u32,
    pub cwd: String,
    pub session_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

pub fn parse_panes(out: &str) -> Vec<PaneInfo> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.splitn(4, '\t');
            let pane_id = it.next()?.to_string();
            let pane_pid: u32 = it.next()?.parse().ok()?;
            let cwd = it.next()?.to_string();
            let session_name = it.next()?.to_string();
            Some(PaneInfo { pane_id, pane_pid, cwd, session_name })
        })
        .collect()
}

pub fn parse_ps(out: &str) -> Vec<ProcEntry> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let ppid: u32 = it.next()?.parse().ok()?;
            let rest = it.collect::<Vec<_>>().join(" ");
            if rest.is_empty() { return None; }
            Some(ProcEntry { pid, ppid, args: rest })
        })
        .collect()
}

pub fn classify(args: &str) -> Option<AgentKind> {
    let argv0 = args.split_whitespace().next()?;
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let is_claude = argv0.contains("/claude/versions/") || base == "claude";
    if is_claude {
        // `claude daemon run` 等常驻进程不是 pane agent
        let is_daemon = args.split_whitespace().nth(1) == Some("daemon");
        return if is_daemon { None } else { Some(AgentKind::Claude) };
    }
    if base == "codex" || base.starts_with("codex-") {
        return Some(AgentKind::Codex);
    }
    None
}

pub fn find_agent(pane_pid: u32, procs: &[ProcEntry]) -> Option<(AgentKind, u32)> {
    // BFS：先看 pane 进程自己，再逐层看子进程，取最浅匹配
    let mut frontier = vec![pane_pid];
    let mut guard = 0;
    while !frontier.is_empty() && guard < 64 {
        guard += 1;
        for &pid in &frontier {
            if let Some(p) = procs.iter().find(|p| p.pid == pid) {
                if let Some(kind) = classify(&p.args) {
                    return Some((kind, pid));
                }
            }
        }
        frontier = procs.iter()
            .filter(|p| frontier.contains(&p.ppid))
            .map(|p| p.pid)
            .collect();
    }
    None
}

pub fn list_panes() -> Vec<PaneInfo> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(paths::tmux_args());
    cmd.args(["list-panes", "-a", "-F", "#{pane_id}\t#{pane_pid}\t#{pane_current_path}\t#{session_name}"]);
    match cmd.output() {
        Ok(out) if out.status.success() => parse_panes(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

pub fn list_procs() -> Vec<ProcEntry> {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-axo", "pid=,ppid=,args="]);
    match cmd.output() {
        Ok(out) if out.status.success() => parse_ps(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}
```

`src/main.rs` 加 `#[allow(dead_code)] // consumed by the scanner loop from Task 4 onward` `mod scanner;`。

- [ ] **Step 4: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: 全绿无 warning。

- [ ] **Step 5: Commit**

```bash
git add src/scanner/ src/main.rs
git commit -m "feat(m2): pane and process discovery with agent classification"
```

---

### Task 4: scanner reconcile 循环接入 daemon

**Files:**
- Modify: `src/scanner/mod.rs`（reconcile 循环）
- Modify: `src/daemon/mod.rs`（起 scanner 线程）
- Modify: `src/main.rs`（去掉 scanner/sources 的 `#[allow(dead_code)]`）
- Test: `tests/scanner_e2e.rs`（隔离 tmux 集成测试，沿用 tests/e2e.rs 的 `-L tfa-e2e-<pid>` 模式）

**Interfaces:**
- Consumes: Task 1 的 StateStore 新方法、Task 3 的 procs、Task 2 的 claude_jsonl（指标部分本任务先接 hook 已知的 transcript_path，cwd 兜底发现也在此实现）
- Produces:
  - `pub fn spawn(store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>)`（daemon 调用；`TFA_NO_SCAN=1` 时直接 return）
  - `fn scan_interval_ms() -> u64`（TFA_SCAN_INTERVAL_MS，默认 15_000）
  - `pub fn tick(store: &Mutex<StateStore>, cursors: &mut HashMap<String, claude_jsonl::TranscriptCursor>, now_ms: u64) -> bool`（单轮 reconcile，返回是否有变更；抽成 pub 供集成测试直接驱动单轮，不必等周期）

**tick 的一轮流程（顺序即规格）：**
1. `procs::list_panes()`；空列表（tmux 不在/无 pane）→ 仍执行 liveness（全部 entry 判 Dead 的正确行为由 `reconcile_liveness` 的「不在 map = 消失」承担——但 **tmux 调用失败与 tmux 没有 pane 无法区分**，因此 list_panes 失败（非零退出）时本轮直接跳过，避免误杀；实现上让 `list_panes` 失败返回空 vec 时用 `tmux has-session` 结果决定：`lifecycle::tmux_alive()` 为 false → 跳过本轮）。
2. `procs::list_procs()`，对每个 pane `find_agent` → 命中的 `upsert_scanned`。
3. 构造 `live: BTreeMap<pane_id, Option<pid>>`（所有 pane 一项；agent 命中给 Some）→ `reconcile_liveness`。
4. `stale_sweep(now_ms)`。
5. session_name 回填：`panes_needing_name()` ∩ 本轮 pane 表（免额外 tmux 调用）。
6. claude 指标：对每个 kind==Claude 且非 Dead 的 entry：无 transcript_path 时先 `discover_transcript(projects_dir(), cwd)` + `set_transcript`；有则 `read_update` → `Some` 时 `set_metrics`。cursors 以 pane_id 为 key；entry 的 transcript_path 变化时（重置/新会话）丢弃旧 cursor 重建。
7. 任何一步产生变化 → 返回 true，daemon 侧置 dirty（快照落盘由既有维护线程完成）。

`projects_dir()` 放 `src/paths.rs`：

```rust
pub fn projects_dir() -> PathBuf {
    env_path("TFA_CLAUDE_PROJECTS_DIR").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".claude/projects")
    })
}
```

- [ ] **Step 1: 写失败测试**

`tests/scanner_e2e.rs`（骨架沿用 tests/e2e.rs 的隔离模式：临时 TFA_STATE_DIR/TFA_SOCKET、`tmux -L tfa-e2e-<pid>`、假 agent 脚本；测试结束 kill tmux server 与 daemon）：

```rust
//! scanner 集成测试：隔离 tmux server + 假 claude 进程 + 假 transcript。
//! 假 agent：一个 sleep 的 shell 脚本，路径伪装成 .../claude/versions/9.9.9 以命中 classify。

use assert_cmd::cargo::cargo_bin;
use std::process::Command;

struct TestEnv { /* 与 tests/e2e.rs 相同的字段：sock 名、state 临时目录、二进制路径 */ }

// 辅助（完整实现见 tests/e2e.rs 同名逻辑，可复制改造）：
// - fn setup() -> TestEnv：临时目录、导出 TFA_* 环境、tmux -L <name> new-session -d
// - fn tmux(env, args...)、fn tfa_list(env) -> serde_json::Value
// - fn wait_until(pred, timeout_ms)：100ms 轮询
// - impl Drop for TestEnv：tmux kill-server、杀 daemon

#[test]
fn scanner_backfills_preexisting_agent_and_marks_dead() {
    let env = setup();
    // 1. daemon 启动前就存在的「存量」假 claude pane（sidebar 盲区场景）
    let fake_home = env.state_dir.path().join("fakehome");
    let agent_dir = fake_home.join(".local/share/claude/versions");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("9.9.9"), "#!/bin/sh\nsleep 300\n").unwrap();
    /* chmod +x */
    // transcript fixture：cwd 编码目录 + 一条真实形状 assistant 行
    let cwd = env.state_dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let proj_dir = env.projects_dir.join(/* encode_cwd(cwd) —— 测试里内联同规则实现 */);
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("s1.jsonl"), format!("{REAL_ASSISTANT_LINE}\n")).unwrap();

    tmux(&env, &["new-window", "-c", cwd.to_str().unwrap()]);
    tmux(&env, &["send-keys", "-t", ":1", &format!("exec {}", agent_dir.join("9.9.9").display()), "Enter"]);

    // 2. 起 daemon（TFA_SCAN_INTERVAL_MS=300 加速）
    /* spawn daemon with envs */

    // 3. 断言：无任何 hook 事件，scanner 建档 + 指标就位
    wait_until(|| {
        let sessions = tfa_list(&env);
        sessions.as_array().is_some_and(|a| a.iter().any(|s|
            s["agent"] == "claude"
            && s["source"] == "scan"
            && s["state"] == "working"
            && s["model"] == "claude-fable-5"
            && s["context"]["percent"] == 98
        ))
    }, 5000);

    // 4. 杀掉假 agent 窗口 → Dead 纠偏（M1 幽灵 working 的根治验证）
    tmux(&env, &["kill-window", "-t", ":1"]);
    wait_until(|| {
        let sessions = tfa_list(&env);
        sessions.as_array().is_some_and(|a| a.iter().any(|s| s["state"] == "dead"))
    }, 5000);
}

#[test]
fn hook_entry_gets_confirmed_to_both_and_stale_working_flagged() {
    // hook 报一个真实存在的 pane（source hook）→ scanner 升级 Both 并回填 session_name；
    // hook 报一个不存在的 pane → reconcile 判 Dead。
    let env = setup();
    /* tfa hook claude user-prompt-submit，TMUX_PANE 分别取真 pane 与 "%999" */
    wait_until(|| {
        let sessions = tfa_list(&env);
        sessions.as_array().is_some_and(|a|
            a.iter().any(|s| s["source"] == "both" && s["session_name"].is_string())
            && a.iter().any(|s| s["pane_id"] == "%999" && s["state"] == "dead"))
    }, 5000);
}
```

（测试文件写完整可运行代码——上面省略号处照抄 tests/e2e.rs 的现成辅助并适配；REAL_ASSISTANT_LINE 用 Task 2 的 fixture 字符串。）

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo test --test scanner_e2e 2>&1 | tail -10`
Expected: 失败——daemon 没有 scanner，`source=="scan"` 的断言超时。存档 RED 证据。

- [ ] **Step 3: 实现**

`src/scanner/mod.rs`：

```rust
pub mod procs;

use crate::sources::claude_jsonl::{self, TranscriptCursor};
use crate::state::StateStore;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn scan_interval_ms() -> u64 {
    std::env::var("TFA_SCAN_INTERVAL_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(15_000)
}

pub fn spawn(store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    if std::env::var("TFA_NO_SCAN").as_deref() == Ok("1") { return; }
    std::thread::spawn(move || {
        let mut cursors: HashMap<String, TranscriptCursor> = HashMap::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(scan_interval_ms()));
            if tick(&store, &mut cursors, crate::daemon::now_ms()) {
                dirty.store(true, Ordering::Relaxed);
            }
        }
    });
}

pub fn tick(
    store: &Mutex<StateStore>,
    cursors: &mut HashMap<String, TranscriptCursor>,
    now_ms: u64,
) -> bool {
    let panes = procs::list_panes();
    if panes.is_empty() && !crate::daemon::lifecycle::tmux_alive() {
        return false; // tmux 不可用与「没有 pane」无法区分，跳过本轮避免误杀
    }
    let procs_list = procs::list_procs();

    let mut live: BTreeMap<String, Option<u32>> = BTreeMap::new();
    let mut matches: Vec<(procs::PaneInfo, crate::event::AgentKind, u32)> = Vec::new();
    for pane in &panes {
        match procs::find_agent(pane.pane_pid, &procs_list) {
            Some((kind, pid)) => {
                live.insert(pane.pane_id.clone(), Some(pid));
                matches.push((pane.clone(), kind, pid));
            }
            None => { live.insert(pane.pane_id.clone(), None); }
        }
    }

    // —— 状态 reconcile（一次锁内完成）——
    let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (pane, kind, pid) in &matches {
        st.upsert_scanned(&pane.pane_id, kind.clone(), *pid, &pane.cwd, &pane.session_name, now_ms);
    }
    st.reconcile_liveness(&live, now_ms);
    st.stale_sweep(now_ms);
    // session_name 回填（用本轮 pane 表，不再额外调 tmux）
    let by_pane: HashMap<&str, &str> = panes.iter()
        .map(|p| (p.pane_id.as_str(), p.session_name.as_str())).collect();
    for pane_id in st.panes_needing_name() {
        if let Some(name) = by_pane.get(pane_id.as_str()) {
            st.set_session_name(&pane_id, name.to_string());
        }
    }
    // —— claude 指标 ——
    let claude_targets: Vec<(String, Option<String>, Option<String>)> = st.sessions().iter()
        .filter(|s| matches!(s.agent, crate::event::AgentKind::Claude)
            && !matches!(s.state, crate::state::SessionState::Dead))
        .map(|s| (s.pane_id.clone(), s.transcript_path.clone(), s.cwd.clone()))
        .collect();
    drop(st);

    for (pane_id, transcript, cwd) in claude_targets {
        let path = match transcript {
            Some(t) => std::path::PathBuf::from(t),
            None => {
                let Some(cwd) = cwd.as_deref() else { continue };
                let Some(found) = claude_jsonl::discover_transcript(&crate::paths::projects_dir(), cwd) else { continue };
                store.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_transcript(&pane_id, found.to_string_lossy().into_owned());
                found
            }
        };
        let cursor = cursors.entry(pane_id.clone()).or_default();
        if let Some(m) = claude_jsonl::read_update(&path, cursor) {
            store.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_metrics(&pane_id, m);
        }
    }
    true
}
```

`src/daemon/mod.rs` 在起维护线程之后加：

```rust
    crate::scanner::spawn(Arc::clone(&store), Arc::clone(&dirty));
```

（`daemon::lifecycle` 需 `pub` 可见：`pub mod lifecycle;` M1 已是 pub。）

注意 cursor 失效规则：transcript_path 被 SessionStart 重置后（entry.transcript_path 变 None），下一轮 discover 会重新绑定；cursors 的旧 offset 属于旧文件——在 `tick` 开头对每个 pane 比对 entry.transcript_path 与上轮绑定（cursors 旁挂 `HashMap<String, PathBuf>` 记录 pane→path），路径变了就 `cursors.remove(pane_id)`。实现放 `tick` 内，测试由 scanner_e2e 的 SessionStart 场景覆盖（可选断言）。

- [ ] **Step 4: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -8`
Expected: 全绿；既有 daemon_lifecycle/daemon_socket/e2e 不回归（若受 scanner 干扰，给它们的 daemon 启动加 `TFA_NO_SCAN=1`——这是允许的既有行为隔离手段）。

- [ ] **Step 5: Commit**

```bash
git add src/scanner/mod.rs src/daemon/mod.rs src/paths.rs src/main.rs tests/scanner_e2e.rs
git commit -m "feat(m2): scanner reconcile loop — backfill, liveness, stale sweep, claude metrics"
```

---

### Task 5: codex sqlite 读取器（sources/codex_db.rs）

**Files:**
- Modify: `Cargo.toml`（加 rusqlite）
- Create: `src/sources/codex_db.rs`
- Modify: `src/sources/mod.rs`（`pub mod codex_db;`）
- Modify: `src/scanner/mod.rs`（tick 里接 codex 指标）

**Interfaces:**
- Consumes: Task 1 的 `SessionMetrics/TokenTotals`
- Produces:
  - `pub struct CodexThread { pub cwd: String, pub model: Option<String>, pub tokens_used: u64, pub updated_at_ms: u64, pub title: String }`
  - `pub fn db_path() -> std::path::PathBuf`（TFA_CODEX_DB 覆写，默认 `$HOME/.codex/state_5.sqlite`）
  - `pub fn load_recent(limit: usize) -> Vec<CodexThread>`（只读打开；库缺失/表缺失/空表/任何错误→空 vec）
  - `pub fn metrics_for(threads: &[CodexThread], cwd: &str) -> Option<SessionMetrics>`（cwd 精确匹配，取 updated_at_ms 最新；model→model，tokens_used→`TokenTotals { total, 其余 0 }`，context=None，git_branch=None）

- [ ] **Step 1: 加依赖**

`Cargo.toml` `[dependencies]` 追加：

```toml
rusqlite = { version = "0.37", features = ["bundled"] }
```

Run: `cargo build 2>&1 | tail -3` Expected: 编译通过（bundled 首次编译较慢属正常）。

- [ ] **Step 2: 写失败测试**

`src/sources/codex_db.rs` 尾部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 照抄本机 state_5.sqlite 的 threads 建表语句（列子集含全部被查询列）
    fn fixture_db(rows: &[(&str, &str, &str, i64, i64, &str)]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(f.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL DEFAULT '', model_provider TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
                sandbox_policy TEXT NOT NULL DEFAULT '', approval_mode TEXT NOT NULL DEFAULT '',
                tokens_used INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0,
                model TEXT, updated_at_ms INTEGER
            );",
        ).unwrap();
        for (id, cwd, model, tokens, updated_ms, title) in rows {
            conn.execute(
                "INSERT INTO threads (id, cwd, model, tokens_used, updated_at_ms, title) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![id, cwd, model, tokens, updated_ms, title],
            ).unwrap();
        }
        f
    }

    #[test]
    fn load_recent_reads_fixture_readonly() {
        let f = fixture_db(&[
            ("t1", "/proj/a", "gpt-5.3-codex", 1234, 1000, "old"),
            ("t2", "/proj/a", "gpt-5.3-codex", 9999, 2000, "new"),
            ("t3", "/proj/b", "gpt-5.3-codex", 42, 1500, "other"),
        ]);
        std::env::set_var("TFA_CODEX_DB", f.path());
        let threads = load_recent(50);
        std::env::remove_var("TFA_CODEX_DB");
        assert_eq!(threads.len(), 3);

        let m = metrics_for(&threads, "/proj/a").unwrap();
        assert_eq!(m.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(m.tokens.unwrap().total, 9999, "同 cwd 取 updated_at_ms 最新");
        assert!(m.context.is_none());
        assert!(metrics_for(&threads, "/nope").is_none());
    }

    #[test]
    fn missing_db_and_empty_table_are_graceful() {
        std::env::set_var("TFA_CODEX_DB", "/nonexistent/state.sqlite");
        assert!(load_recent(10).is_empty());
        std::env::remove_var("TFA_CODEX_DB");
        let f = fixture_db(&[]);
        std::env::set_var("TFA_CODEX_DB", f.path());
        assert!(load_recent(10).is_empty()); // 本机现状：0 行
        std::env::remove_var("TFA_CODEX_DB");
    }
}
```

（env-var 测试与 paths.rs 同理有并发竞态——两个测试都动 TFA_CODEX_DB，合并进单个 `#[test]` 或用同一把测试内顺序执行：照 paths.rs 先例合并为一个 `#[test] fn codex_db_reader_suite()`。）

- [ ] **Step 3: 跑测试确认 RED**

Run: `cargo test codex 2>&1 | tail -10`
Expected: 编译错误（模块不存在）。存档 RED 证据。

- [ ] **Step 4: 实现**

`src/sources/codex_db.rs`：

```rust
use crate::state::{SessionMetrics, TokenTotals};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct CodexThread {
    pub cwd: String,
    pub model: Option<String>,
    pub tokens_used: u64,
    pub updated_at_ms: u64,
    pub title: String,
}

pub fn db_path() -> PathBuf {
    match std::env::var_os("TFA_CODEX_DB").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".codex/state_5.sqlite")
        }
    }
}

pub fn load_recent(limit: usize) -> Vec<CodexThread> {
    let path = db_path();
    if !path.exists() { return Vec::new(); }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT cwd, model, tokens_used, COALESCE(updated_at_ms, updated_at * 1000), title
         FROM threads WHERE archived = 0
         ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC LIMIT ?1",
    ) else { return Vec::new() };
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(CodexThread {
            cwd: r.get(0)?,
            model: r.get(1)?,
            tokens_used: r.get::<_, i64>(2)?.max(0) as u64,
            updated_at_ms: r.get::<_, i64>(3)?.max(0) as u64,
            title: r.get(4)?,
        })
    });
    match rows {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn metrics_for(threads: &[CodexThread], cwd: &str) -> Option<SessionMetrics> {
    let t = threads.iter().filter(|t| t.cwd == cwd).max_by_key(|t| t.updated_at_ms)?;
    Some(SessionMetrics {
        model: t.model.clone(),
        context: None,
        tokens: Some(TokenTotals { total: t.tokens_used, ..Default::default() }),
        git_branch: None,
    })
}
```

`src/scanner/mod.rs` 的 `tick` 在 claude 指标段之后追加 codex 段：

```rust
    // —— codex 指标 ——
    let codex_targets: Vec<(String, Option<String>)> = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.sessions().iter()
            .filter(|s| matches!(s.agent, crate::event::AgentKind::Codex)
                && !matches!(s.state, crate::state::SessionState::Dead))
            .map(|s| (s.pane_id.clone(), s.cwd.clone()))
            .collect()
    };
    if !codex_targets.is_empty() {
        let threads = crate::sources::codex_db::load_recent(200);
        if !threads.is_empty() {
            let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (pane_id, cwd) in codex_targets {
                let Some(cwd) = cwd.as_deref() else { continue };
                if let Some(m) = crate::sources::codex_db::metrics_for(&threads, cwd) {
                    st.set_metrics(&pane_id, m);
                }
            }
        }
    }
```

- [ ] **Step 5: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -5`
Expected: 全绿无 warning。

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/sources/ src/scanner/mod.rs
git commit -m "feat(m2): codex thread metrics via read-only sqlite reader"
```

---

### Task 6: 加固批（socket 目录、Mutex poisoning、activity marker 清理、Custom wire 钉形）

**Files:**
- Modify: `src/paths.rs`（XDG_RUNTIME_DIR）
- Modify: `src/daemon/mod.rs`（socket 目录 0700 + 属主校验；poisoning）
- Modify: `src/daemon/server.rs`、`src/daemon/lifecycle.rs`（poisoning；marker 清理）
- Modify: `src/event.rs`（Custom wire 钉形测试）
- Test: 各文件模块内 + `tests/daemon_lifecycle.rs` 追加

**Interfaces:**
- Produces:
  - `paths::socket_path()` 新优先级：TFA_SOCKET > `$XDG_RUNTIME_DIR/tfa/tfa.sock`（env 存在且非空时）> `/tmp/tfa-<uid>/tfa.sock`
  - `daemon` 内部 `fn ensure_socket_dir(dir: &Path) -> anyhow::Result<()>`：create_dir_all + `set_permissions(0o700)` + metadata.uid == getuid 校验，不符 bail
  - `lifecycle::clean_activity_markers(now_ms: u64)`：删除 state_dir 下 mtime 距今 > 3600_000ms 的 `activity-*` 文件；由维护线程每轮调用
  - 全仓 `.lock().unwrap()` → `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)`（daemon/mod.rs、server.rs、lifecycle.rs；Task 4/5 新代码已按此写）

- [ ] **Step 1: 写失败测试**

`src/paths.rs` 测试函数内追加（保持单函数防竞态）：

```rust
        // Test 10: XDG_RUNTIME_DIR 存在时优先于 /tmp 默认
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/501");
        assert_eq!(socket_path(), PathBuf::from("/run/user/501/tfa/tfa.sock"));
        // TFA_SOCKET 仍然最高优先
        std::env::set_var("TFA_SOCKET", "/x/y.sock");
        assert_eq!(socket_path(), PathBuf::from("/x/y.sock"));
        std::env::remove_var("TFA_SOCKET");
        std::env::remove_var("XDG_RUNTIME_DIR");
```

`src/event.rs` 测试追加：

```rust
    #[test]
    fn custom_agent_kind_wire_shape_is_pinned() {
        // 对外 wire 契约（M2 定型）：custom agent 序列化为 {"custom":"<name>"}
        let json = serde_json::to_string(&AgentKind::Custom("hermes".into())).unwrap();
        assert_eq!(json, r#"{"custom":"hermes"}"#);
        assert_eq!(serde_json::to_string(&AgentKind::Claude).unwrap(), r#""claude""#);
    }
```

`tests/daemon_lifecycle.rs` 追加（沿用该文件现有 daemon 启停辅助）：

```rust
#[test]
fn socket_dir_perms_are_0700_and_stale_activity_markers_pruned() {
    // 起 daemon（隔离 TFA_STATE_DIR/TFA_SOCKET，TFA_SKIP_TMUX_CHECK=1, TFA_NO_SCAN=1,
    // TFA_TMUX_CHECK_INTERVAL_MS=200）
    // 1. socket 父目录权限 == 0o700
    // 2. 预置 state_dir/activity-old（mtime 拨旧 2h，用 File::set_modified）与 activity-new
    //    等两个维护周期后：old 消失、new 仍在
    use std::os::unix::fs::PermissionsExt;
    /* ... 断言 sock_dir.metadata().permissions().mode() & 0o777 == 0o700 ... */
    /* ... wait_until(|| !old_marker.exists(), 3000) && new_marker.exists() ... */
}
```

（写成完整可运行代码，辅助函数照抄同文件既有测试。）

- [ ] **Step 2: 跑测试确认 RED**

Run: `cargo test --test daemon_lifecycle 2>&1 | tail -8 && cargo test paths 2>&1 | tail -5 && cargo test event 2>&1 | tail -5`
Expected: XDG 断言失败（返回 /tmp 默认）；0700 断言失败（目前是默认 umask 权限）；marker 清理超时失败；Custom wire 测试直接通过的话说明形状已符合——保留为钉形测试（该条 RED 免除）。存档 RED 证据。

- [ ] **Step 3: 实现**

`src/paths.rs`：

```rust
pub fn socket_path() -> PathBuf {
    if let Some(p) = env_path("TFA_SOCKET") { return p; }
    if let Some(runtime) = env_path("XDG_RUNTIME_DIR") {
        return runtime.join("tfa/tfa.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/tfa-{uid}/tfa.sock"))
}
```

`src/daemon/mod.rs` 的 `run()` 里，socket 父目录处理改为：

```rust
    if let Some(parent) = sock_path.parent() {
        ensure_socket_dir(parent)?;
    }
```

```rust
fn ensure_socket_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let meta = std::fs::metadata(dir)?;
    let uid = unsafe { libc::getuid() };
    anyhow::ensure!(meta.uid() == uid, "socket dir {} owned by uid {}, expected {}", dir.display(), meta.uid(), uid);
    Ok(())
}
```

`src/daemon/lifecycle.rs`：

```rust
const MARKER_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

pub fn clean_activity_markers() {
    let Ok(entries) = std::fs::read_dir(crate::paths::state_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("activity-") { continue; }
        let stale = entry.metadata().and_then(|m| m.modified()).ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > MARKER_MAX_AGE);
        if stale { let _ = std::fs::remove_file(entry.path()); }
    }
}
```

维护线程循环里（daemon/mod.rs，prune 之后）加 `lifecycle::clean_activity_markers();`。
全仓替换 `.lock().unwrap()`（rg 检查：`rg -n "lock\(\)\.unwrap\(\)" src/` 结果必须为零）。

- [ ] **Step 4: 跑全量测试确认 GREEN**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -3`
Expected: 全绿；clippy 无新告警。

- [ ] **Step 5: Commit**

```bash
git add src/paths.rs src/daemon/ src/event.rs tests/daemon_lifecycle.rs
git commit -m "feat(m2): hardening — socket dir perms, poison recovery, marker pruning, wire pin"
```

---

### Task 7: 端到端验证与文档

**Files:**
- Modify: `tests/e2e.rs`（快照升级兼容场景）
- Modify: `README.md`（新环境变量、指标字段说明）

**Interfaces:** 无新接口；纯验证 + 文档。

- [ ] **Step 1: 写失败测试（M1→M2 快照升级 e2e）**

`tests/e2e.rs` 追加：

```rust
#[test]
fn daemon_loads_m1_era_snapshot_and_serves_enriched_shape() {
    // 1. 预写 M1 形状的 snapshot.json 到隔离 TFA_STATE_DIR（无 source/model 等字段）
    // 2. 起 daemon（TFA_NO_SCAN=1, TFA_SKIP_TMUX_CHECK=1）
    // 3. tfa list：条目存在，"source":"hook"（default 补齐），无 panic
    /* 完整代码沿用本文件既有辅助 */
}
```

- [ ] **Step 2: RED**

Run: `cargo test --test e2e 2>&1 | tail -8`
Expected: 若 Task 1 serde default 正确则直接 GREEN——允许：该测试是端到端契约钉子，RED 免除条件同 Task 6 的钉形测试；若失败则暴露真 bug，修复后再过。

- [ ] **Step 3: README 更新**

`README.md` 环境变量表追加 TFA_SCAN_INTERVAL_MS / TFA_CLAUDE_PROJECTS_DIR / TFA_CODEX_DB / TFA_NO_SCAN 四行 + 指标字段（model/context/tokens/git_branch/source）在 `tfa list` 输出中的含义说明（各一句话）。

- [ ] **Step 4: 全量回归**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add tests/e2e.rs README.md
git commit -m "test(m2): snapshot upgrade e2e + document new env knobs and metric fields"
```

---

### Task 8: 本机真装验收（用户执行，无 subagent）

**Files:** 无代码变更（cargo install + 观察）

- [ ] **Step 1: 控制器执行安装**

```bash
cargo install --path . --force
tmux display-message "tfa m2 installed"
```

daemon 换血：`pkill -f 'tfa daemon'`（下一个 hook/status 调用自动拉起新版，快照恢复）。

- [ ] **Step 2: 用户验收清单（逐项确认）**

1. `tfa list | python3 -m json.tool`：既有 claude 会话出现 `model` / `context.percent` / `tokens.total`（等一个扫描周期 ≤15s）。
2. 新开一个 tmux 窗口跑 `claude`，**不发消息直接看**：≤15s 内 `tfa list` 出现 `source:"scan"` 条目（存量建档验证——M1 做不到的）。
3. kill 一个 claude 窗口：≤15s 内该条目变 `dead`，5 分钟后消失（幽灵 working 根治验证）。
4. 状态栏 `⚡/⏸/✓` 行为与 M1 一致（无 UI 变化，M4 才动）。
5. codex：跑一个 codex 会话（若 threads 表仍为空，确认 `tfa list` 的 codex 条目生命周期正常、指标字段为 null 不报错）。

- [ ] **Step 3: 用户说「验收通过」后**，走 superpowers:finishing-a-development-branch 合并。

---

## Self-Review 记录

- **Spec coverage**：spec §4 scanner 职责（建档/纠偏/资源指标）→ Task 1/3/4；§5 数据模型（context/tokens/model/source）→ Task 1（git 状态收窄为 JSONL 免费送的 gitBranch，repo_root/git_dirty 与 cost_estimate 留给 M3+，记入开放决策）；§6 claude JSONL 尾部增量读 → Task 2；§6 codex sqlite 现场探明 → 已完成（threads 表 schema 实测入 fixture），reader → Task 5；§8 错误处理表（半写/坏行/单源失效）→ Task 2/5 测试显式覆盖；台账 M2 项 1-7 → Task 1（漂移/重置）、4（scanner/超时根治=名字回填不再走热路径新增调用）、5（codex）、6（socket/poison/marker/wire）。
- **Placeholder scan**：Task 4/6/7 的集成测试骨架标注「照抄 tests/e2e.rs 既有辅助」——e2e.rs 在仓库内且实现者可读，属于引用现有代码而非占位；其余步骤均给全代码。
- **Type consistency**：SessionMetrics/TokenTotals/ContextUsage 在 Task 1 定义，Task 2/4/5 消费处签名一致；`tick` 的 cursors 类型与 Task 2 的 TranscriptCursor 一致；`upsert_scanned` 六参签名在 Task 1 测试与 Task 4 调用一致。

## 开放决策（默认按推荐走，验收时可翻案）

1. 扫描周期默认 15s（心跳靠 hook，扫描只兜底）——可调 TFA_SCAN_INTERVAL_MS。
2. context 窗口表：fable→1M、其余 claude→200k、未知→None（只显示绝对量）。模型窗口变化时改一处常量。
3. cost_estimate 与订阅配额留给 M3（配额里程碑本来就要建价格/额度模型）。
4. tmux 状态栏一行在 M2 不加指标（UI 形态 M4 定）；指标先经 `tfa list`/`--format json` 交付。
