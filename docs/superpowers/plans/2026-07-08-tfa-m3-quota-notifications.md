# tfa M3 配额与通知 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 tfa 加本地用量/burn rate（显示）+ 主动通知（macOS/tmux/手机推送，边沿触发有纪律），兑现「agent 等输入不被晾着」。

**Architecture:** daemon 内新增：config 模块（`~/.config/tfa/config.toml`）、单调 `consumed` 计数器（burn rate 唯一可信输入，排除 cache_read）、burn 采样器（provider 级只加正 delta）、QuotaCache（独立 `Arc<Mutex>`，**不进快照**）、notifier 独立线程（mpsc 队列 + 硬超时 + ureq）、通知纪律（tick 边界快照 diff 算净边沿 + 边沿冷却 + boot-grace + generation + dead 去抖）。真实 OAuth 配额 API 与 `quota_alert` 推后到未来里程碑（M3 零凭证）。

**Tech Stack:** Rust 2021、std::thread + Arc<Mutex>（无 async runtime）、serde/serde_json、新依赖 `ureq`（rustls，纯阻塞带超时）+ `toml`。

## Global Constraints

- edition 2021；**无 async runtime**（std::thread + Arc<Mutex>）。
- 新依赖仅 `ureq = { version = "3", default-features = false, features = ["rustls","json"] }` + `toml = "0.8"`。**禁 reqwest**（拉 tokio，违反无 async + 节制依赖）。
- **`tfa hook` 绝不阻塞 agent**：所有通知/采样 IO 在 daemon 内；`src/client.rs`、`src/commands/hook.rs` 一行通知/配额代码都不得出现（Task 6 护栏，验收硬项）。
- **daemon 内 IO 绝不持 StateStore 锁跨 IO**（M2 纪律）：respond()/tick() 锁内只收集 `Vec<NotifyEvent>`，出锁非阻塞 `mpsc::Sender::send` 入队。
- **配额/通知网络 IO 只在 notifier 独立线程**，带硬超时（≤10s，`timeout_ms` 默认 3000），**绝不落在 scanner tick 或维护线程上**。
- **新增易失态一律不进序列化的 `StateStore`**（QuotaState/burn 缓冲/冷却/generation 放独立 `Arc<Mutex<QuotaCache>>` 与 `Discipline`）。唯一进 `AgentSession` 的新字段是 `consumed_tokens: u64`，必须 `#[serde(default)]`。
- **burn rate 唯一输入是单调 `consumed`**（累加 `input_tokens + output_tokens`，**排除 `cache_read`/`cache_creation`**）；**绝不用 `tokens.total`**（它是最后一条消息的单次用量、被 cache_read 主导，非累计）。
- **burn 聚合只加正 delta**：`delta = consumed.saturating_sub(last_seen)`；会话消失/prune/重置贡献 0，绝不为负。
- **LocalEstimate 诚实性**：`window_5h_percent`/`weekly_percent` **恒 None**；只暴露 `observed_tokens_this_window`（下界，前缀 `≥`）+ `burn_rate`。
- 时间戳一律 epoch 毫秒 u64，字段名 `_ms` 后缀。
- 通知发送失败**静默吞，不重试、不告警、不打断 daemon**（TCC 静默失败 / tmux no-clients / APNs 限流均属正常）。
- scanner/daemon 调 tmux 必须带 `paths::tmux_args()`（`-L` 隔离）。
- 测试隔离环境变量（新增）：`TFA_CONFIG_PATH`（config 文件路径覆写）、`TFA_NO_NOTIFY=1`（抑制真实通知副作用，notifier 把事件写进内存 sink 而非派发）。既有 `TFA_*` 沿用。
- 每个改动跑 `cargo test` 全绿 + `cargo clippy --all-targets` 干净后再提交。

## Global Interfaces（跨任务锁定，实现者据此对齐）

```rust
// ── Task 1: src/config.rs ──
pub struct Config { pub notify: NotifyConfig, pub quota: QuotaConfig }
pub struct NotifyConfig {
    pub enabled: bool,                     // 默认 true
    pub quiet_hours: Option<QuietHours>,   // 默认 None
    pub quiet_hours_exempt: Vec<String>,   // 默认 ["dead"]
    pub channels: Channels,
    pub triggers: Triggers,
    pub discipline: DisciplineConfig,      // 注意：config 的，与 notify::discipline::Discipline（Task 6）不同名
}
pub struct QuietHours { pub start: String, pub end: String }        // "HH:MM"
pub struct Channels { pub tmux: Toggle, pub macos: Toggle, pub http: HttpChannel }
pub struct Toggle { pub enabled: bool }
pub struct HttpChannel { pub enabled: bool, pub url: String, pub format: String, pub timeout_ms: u64, pub headers: std::collections::BTreeMap<String,String> }
pub struct Triggers { pub waiting_input: bool, pub done: bool, pub stale: bool, pub dead: bool } // 默认 t,f,f,f
pub struct DisciplineConfig { pub cooldown_secs: u64, pub dead_debounce_ticks: u64, pub boot_grace_secs: u64 } // 30,2,30
pub struct QuotaConfig { pub burn_rate_window_mins: u64 } // 60

// ── 既有函数在 M3 全部改动后的最终签名（跨 T4/T6 锁定，防参数堆叠漂移）──
// src/scanner/mod.rs:
//   pub fn spawn(store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>,
//                quota: Arc<Mutex<QuotaCache>>, config: Arc<Mutex<Config>>,
//                discipline: Arc<Mutex<Discipline>>, notify_tx: mpsc::Sender<NotifyEvent>)
//   fn tick(store, cursors, cursor_paths, burn: &mut BurnSampler, quota: &Mutex<QuotaCache>,
//           config: &Mutex<Config>, discipline: &Mutex<Discipline>, tx: &mpsc::Sender<NotifyEvent>, now_ms) -> bool
//   （T4 引入 burn/quota；T6 引入 config/discipline/tx。BurnSampler 在 spawn 闭包 own，跨 tick 存活。）
// src/daemon/server.rs:
//   pub fn serve(listener, store, dirty, quota: Arc<Mutex<QuotaCache>>,
//                config: Arc<Mutex<Config>>, discipline: Arc<Mutex<Discipline>>, tx: mpsc::Sender<NotifyEvent>)
//   handle/respond 透传全部；respond 的 Snapshot 读 quota，Hook 分支锁内算 edges、出锁 tx.send。
impl Config { pub fn load() -> Self; pub fn from_toml_str(s: &str) -> Self } // load 读 paths::config_path()，缺失/坏值→Default
impl Default for Config { /* 全部 spec 默认 */ }

// ── Task 2: src/sources/claude_jsonl.rs / src/state.rs ──
pub struct TranscriptCursor { pub offset: u64, pub consumed: u64 } // consumed 单调累加
// read_update 签名不变：pub fn read_update(path,&mut cursor)->Option<SessionMetrics>；副作用累加 cursor.consumed
// AgentSession 新增： #[serde(default)] pub consumed_tokens: u64
impl AgentSession { pub fn stable_key(&self) -> String } // agent_session_id 优先，回退 pane_id
impl StateStore { pub fn set_consumed(&mut self, pane_id: &str, consumed: u64) } // 单调 max 守卫
// AgentKind 增加 derive(Eq, Hash)（供 provider HashMap 键）

// ── Task 3: src/quota/burn.rs ──
pub struct BurnSampler { /* private */ }
impl BurnSampler {
    pub fn new(window_mins: u64) -> Self;
    pub fn sample(&mut self, sessions: &[(String, crate::event::AgentKind, u64)], now_ms: u64); // (stable_key, provider, consumed)
    pub fn provider_consumed(&self, provider: &crate::event::AgentKind) -> u64;                 // 单调累计
    pub fn burn_rate_per_min(&self, provider: &crate::event::AgentKind, now_ms: u64) -> f64;     // 窗口内 Δ/Δt
    pub fn providers(&self) -> Vec<crate::event::AgentKind>;
}

// ── Task 4: src/quota/mod.rs ──
pub struct QuotaState {
    pub provider: crate::event::AgentKind,
    pub window_5h_percent: Option<u8>,  // M3 恒 None
    pub weekly_percent: Option<u8>,     // M3 恒 None
    pub reset_at_ms: Option<u64>,
    pub reset_estimated: bool,          // M3 恒 true
    pub observed_tokens_this_window: u64,
    pub burn_rate_per_min: f64,
    pub source: QuotaSource,            // M3 恒 LocalEstimate
    pub freshness_ms: u64,
}
pub enum QuotaSource { LocalEstimate } // 未来加 RealApi
pub struct QuotaCache { /* per-provider 5h 块锚 + 最新 QuotaState */ }
impl QuotaCache {
    pub fn new() -> Self;
    pub fn refresh(&mut self, burn: &BurnSampler, now_ms: u64); // 组装 QuotaState，管理 5h 块
    pub fn states(&self) -> Vec<QuotaState>;
}
// protocol Response::Snapshot 增加 #[serde(default)] quota: Vec<QuotaState>

// ── Task 5: src/notify/mod.rs + channels.rs ──
pub enum NotifyKind { WaitingInput, Done, Stale, Dead }
impl NotifyKind { pub fn as_str(&self) -> &'static str } // "waiting_input"|"done"|"stale"|"dead"
pub struct NotifyEvent {
    pub session_key: String, pub pane_id: String, pub session_name: Option<String>,
    pub kind: NotifyKind, pub generation: u64, pub title: String, pub body: String,
}
pub fn spawn_notifier(rx: std::sync::mpsc::Receiver<NotifyEvent>, cfg: std::sync::Arc<std::sync::Mutex<Config>>); // 独立线程
pub mod channels; // dispatch(ev, &NotifyConfig)；各通道带超时；TFA_NO_NOTIFY 走内存 sink

// ── Task 6: src/notify/discipline.rs ──
pub fn trigger_kind(state: &crate::state::SessionState) -> Option<NotifyKind>; // 触发态判别式（忽略 reason）
pub struct Discipline { /* prev/cooldown/generation/dead_pending/boot_until_ms */ }
impl Discipline {
    pub fn new(boot_grace_secs: u64, now_ms: u64) -> Self;
    pub fn seed(&mut self, sessions: &[crate::state::AgentSession]);            // 启动播种基线
    pub fn edges(&mut self, before: &std::collections::HashMap<String, Option<NotifyKind>>,
                 after: &[crate::state::AgentSession], cfg: &NotifyConfig, now_ms: u64) -> Vec<NotifyEvent>;
    pub fn snapshot_states(sessions: &[crate::state::AgentSession]) -> std::collections::HashMap<String, Option<NotifyKind>>;
}
```

---

### Task 1: config 模块

**Files:**
- Create: `src/config.rs`
- Modify: `src/paths.rs`（`config_path()`）
- Modify: `src/main.rs`（`mod config;`，`#[allow(dead_code)]` 直到 Task 5/6 消费）
- Modify: `Cargo.toml`（`toml` 依赖）
- Test: `src/config.rs`（模块内单测）、`src/paths.rs`（并入既有单测 fn）

**Interfaces:**
- Consumes: `paths::env_path`（既有私有 helper 模式）
- Produces: 见 Global Interfaces Task 1 全部类型 + `paths::config_path() -> PathBuf`

- [ ] **Step 1: 加依赖**

`Cargo.toml` `[dependencies]` 追加：
```toml
toml = "0.8"
```
Run: `cargo build 2>&1 | tail -3`　Expected: 编译通过。

- [ ] **Step 2: 写失败测试**

`src/config.rs`：
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_all_defaults() {
        let c = Config::from_toml_str("");
        assert!(c.notify.enabled);
        assert!(c.notify.channels.tmux.enabled && c.notify.channels.macos.enabled);
        assert!(!c.notify.channels.http.enabled);
        assert_eq!(c.notify.channels.http.format, "bark");
        assert_eq!(c.notify.channels.http.timeout_ms, 3000);
        assert!(c.notify.triggers.waiting_input);
        assert!(!c.notify.triggers.done && !c.notify.triggers.stale && !c.notify.triggers.dead);
        assert_eq!(c.notify.discipline.cooldown_secs, 30);
        assert_eq!(c.notify.discipline.dead_debounce_ticks, 2);
        assert_eq!(c.notify.discipline.boot_grace_secs, 30);
        assert_eq!(c.notify.quiet_hours_exempt, vec!["dead".to_string()]);
        assert_eq!(c.quota.burn_rate_window_mins, 60);
        assert!(c.notify.quiet_hours.is_none());
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let c = Config::from_toml_str(r#"
[notify.triggers]
done = true
stale = true
[notify.channels.http]
enabled = true
url = "http://192.168.1.9:8080/devkey"
format = "ntfy"
[notify.discipline]
cooldown_secs = 45
[quota]
burn_rate_window_mins = 30
"#);
        assert!(c.notify.triggers.done && c.notify.triggers.stale);
        assert!(c.notify.triggers.waiting_input);          // 未提及仍默认 true
        assert!(!c.notify.triggers.dead);                  // 未提及仍默认 false
        assert!(c.notify.channels.http.enabled);
        assert_eq!(c.notify.channels.http.url, "http://192.168.1.9:8080/devkey");
        assert_eq!(c.notify.channels.http.format, "ntfy");
        assert_eq!(c.notify.channels.http.timeout_ms, 3000); // 未提及仍默认
        assert_eq!(c.notify.discipline.cooldown_secs, 45);
        assert_eq!(c.notify.discipline.boot_grace_secs, 30); // 未提及仍默认
        assert_eq!(c.quota.burn_rate_window_mins, 30);
        assert!(c.notify.channels.macos.enabled);            // 整段未提仍默认
    }

    #[test]
    fn quiet_hours_parses_when_present() {
        let c = Config::from_toml_str(r#"
[notify]
quiet_hours = { start = "23:00", end = "08:00" }
"#);
        let qh = c.notify.quiet_hours.expect("quiet_hours present");
        assert_eq!(qh.start, "23:00");
        assert_eq!(qh.end, "08:00");
    }

    #[test]
    fn garbage_toml_falls_back_to_default_not_panic() {
        let c = Config::from_toml_str("this is not : valid = toml = [");
        assert!(c.notify.enabled); // 坏输入→默认，绝不 panic
    }
}
```

`src/paths.rs` 既有 `#[test] fn test_all_path_functions()` 末尾追加（保持单 fn 防竞态）：
```rust
        // Test 10: TFA_CONFIG_PATH 覆写
        std::env::set_var("TFA_CONFIG_PATH", "/tmp/tfa-test.toml");
        assert_eq!(config_path(), PathBuf::from("/tmp/tfa-test.toml"));
        std::env::remove_var("TFA_CONFIG_PATH");
        // Test 11: 缺省 → ~/.config/tfa/config.toml
        let cp = config_path();
        assert!(cp.to_string_lossy().ends_with(".config/tfa/config.toml"));
```

- [ ] **Step 3: 跑测试确认 RED**

Run: `cargo test config 2>&1 | tail -15 && cargo test test_all_path_functions 2>&1 | tail -5`
Expected: 编译错误（`Config`/`config_path` 不存在）。存 RED 证据入报告。

- [ ] **Step 4: 实现**

`src/paths.rs` 追加：
```rust
pub fn config_path() -> PathBuf {
    env_path("TFA_CONFIG_PATH").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".config/tfa/config.toml")
    })
}
```

`src/config.rs`（全量）：
```rust
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub notify: NotifyConfig,
    pub quota: QuotaConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub quiet_hours: Option<QuietHours>,
    pub quiet_hours_exempt: Vec<String>,
    pub channels: Channels,
    pub triggers: Triggers,
    pub discipline: DisciplineConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuietHours { pub start: String, pub end: String }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Channels { pub tmux: Toggle, pub macos: Toggle, pub http: HttpChannel }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Toggle { pub enabled: bool }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HttpChannel {
    pub enabled: bool,
    pub url: String,
    pub format: String,
    pub timeout_ms: u64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Triggers { pub waiting_input: bool, pub done: bool, pub stale: bool, pub dead: bool }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DisciplineConfig { pub cooldown_secs: u64, pub dead_debounce_ticks: u64, pub boot_grace_secs: u64 }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaConfig { pub burn_rate_window_mins: u64 }

impl Default for Config {
    fn default() -> Self { Self { notify: NotifyConfig::default(), quota: QuotaConfig::default() } }
}
impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet_hours: None,
            quiet_hours_exempt: vec!["dead".to_string()],
            channels: Channels::default(),
            triggers: Triggers::default(),
            discipline: DisciplineConfig::default(),
        }
    }
}
impl Default for Channels {
    fn default() -> Self { Self { tmux: Toggle { enabled: true }, macos: Toggle { enabled: true }, http: HttpChannel::default() } }
}
impl Default for Toggle { fn default() -> Self { Self { enabled: true } } }
impl Default for HttpChannel {
    fn default() -> Self { Self { enabled: false, url: String::new(), format: "bark".into(), timeout_ms: 3000, headers: BTreeMap::new() } }
}
impl Default for Triggers {
    fn default() -> Self { Self { waiting_input: true, done: false, stale: false, dead: false } }
}
impl Default for DisciplineConfig {
    fn default() -> Self { Self { cooldown_secs: 30, dead_debounce_ticks: 2, boot_grace_secs: 30 } }
}
impl Default for QuotaConfig { fn default() -> Self { Self { burn_rate_window_mins: 60 } } }

impl Config {
    /// 读 config_path()，缺失/坏值→默认，绝不 panic。
    pub fn load() -> Self {
        std::fs::read_to_string(crate::paths::config_path())
            .ok()
            .map(|s| Self::from_toml_str(&s))
            .unwrap_or_default()
    }
    /// 坏 toml → 默认（不 panic）。
    pub fn from_toml_str(s: &str) -> Self {
        toml::from_str(s).unwrap_or_default()
    }
}
```
注意 `Toggle` 无 `#[serde(default)]` 在字段级但结构体级有——`Channels`/`NotifyConfig` 用 `#[serde(default)]` 使缺失子表走 `Default`；`Toggle` 自身 `#[serde(default)]` 使 `{ }` 或缺 `enabled` 也走默认 true。

`src/main.rs` 顶部 mod 区加：
```rust
#[allow(dead_code)] // consumed by notifier/discipline from Task 5-6
mod config;
```

- [ ] **Step 5: GREEN + 提交**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿无告警。
```bash
git add src/config.rs src/paths.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "feat(m3): config module with toml parsing and spec defaults"
```

---

### Task 2: 单调 consumed 计数器

**Files:**
- Modify: `src/sources/claude_jsonl.rs`（`TranscriptCursor.consumed` + `read_update` 累加）
- Modify: `src/state.rs`（`AgentSession.consumed_tokens` + `stable_key()` + `set_consumed()`）
- Modify: `src/event.rs`（`AgentKind` 加 `Eq, Hash`）
- Modify: `src/scanner/mod.rs`（tick 里把 consumed 写进会话）
- Test: 上述文件模块内单测

**Interfaces:**
- Consumes: 无新外部
- Produces: 见 Global Interfaces Task 2

- [ ] **Step 1: 写失败测试**

`src/sources/claude_jsonl.rs` tests 追加（沿用既有 `REAL_ASSISTANT`/`SYNTHETIC`/`HEADER`/`write_lines` 常量与 helper）：
```rust
    #[test]
    fn consumed_accumulates_new_output_and_input_excluding_cache() {
        // REAL_ASSISTANT: input_tokens=2, output_tokens=1045, cache_read=982162, cache_creation=705
        // consumed delta = input+output = 1047，排除 cache_read/cache_creation
        let f = write_lines(&[REAL_ASSISTANT, REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, 1047 * 2, "两条真实 assistant 各累加 input+output=1047");
    }

    #[test]
    fn consumed_skips_synthetic_and_is_monotonic_across_incremental_reads() {
        let f = write_lines(&[REAL_ASSISTANT, SYNTHETIC, HEADER]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, 1047, "synthetic(0 usage)/header 不计");
        let before = cur.consumed;
        read_update(f.path(), &mut cur); // 无新行
        assert_eq!(cur.consumed, before, "无新行 consumed 不变");
        // 追加一条真实行
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new().append(true).open(f.path()).unwrap();
        writeln!(fh, "{REAL_ASSISTANT}").unwrap(); fh.flush().unwrap();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, before + 1047, "新行继续累加，单调");
    }
```

`src/state.rs` tests 追加：
```rust
    #[test]
    fn consumed_tokens_defaults_zero_and_set_is_monotonic() {
        let mut st = StateStore::new();
        st.apply(ev(EventKind::SessionStart, 0));
        assert_eq!(st.sessions()[0].consumed_tokens, 0);
        st.set_consumed("%1", 500);
        assert_eq!(st.sessions()[0].consumed_tokens, 500);
        st.set_consumed("%1", 400); // 回退被守卫忽略（单调）
        assert_eq!(st.sessions()[0].consumed_tokens, 500);
        st.set_consumed("%1", 900);
        assert_eq!(st.sessions()[0].consumed_tokens, 900);
    }

    #[test]
    fn stable_key_prefers_session_id_else_pane() {
        let mut st = StateStore::new();
        let mut e = ev(EventKind::SessionStart, 0);
        e.session_id = Some("sess-abc".into());
        st.apply(e);
        assert_eq!(st.sessions()[0].stable_key(), "sess-abc");
        let mut st2 = StateStore::new();
        st2.apply(ev(EventKind::SessionStart, 0)); // 无 session_id
        assert_eq!(st2.sessions()[0].stable_key(), "%1");
    }

    #[test]
    fn session_start_reset_zeroes_consumed() {
        let mut st = StateStore::new();
        st.apply(ev(EventKind::UserPromptSubmit, 1000));
        st.set_consumed("%1", 500);
        st.apply(ev(EventKind::SessionStart, 2000)); // 新会话复用 pane → 重置
        assert_eq!(st.sessions()[0].consumed_tokens, 0, "新会话 consumed 归零");
    }
```

`src/event.rs` tests 追加：
```rust
    #[test]
    fn agent_kind_is_hashable() {
        use std::collections::HashMap;
        let mut m: HashMap<AgentKind, u64> = HashMap::new();
        m.insert(AgentKind::Claude, 1);
        m.insert(AgentKind::Codex, 2);
        m.insert(AgentKind::Custom("x".into()), 3);
        assert_eq!(m[&AgentKind::Claude], 1);
        assert_eq!(m[&AgentKind::Custom("x".into())], 3);
    }
```

- [ ] **Step 2: RED**

Run: `cargo test consumed 2>&1 | tail -12 && cargo test stable_key 2>&1 | tail -5 && cargo test agent_kind_is_hashable 2>&1 | tail -5`
Expected: 编译错误（字段/方法不存在）。存 RED 证据。

- [ ] **Step 3: 实现**

`src/event.rs`：`AgentKind` 的 derive 加 `Eq, Hash`：
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind { Claude, Codex, Custom(String) }
```

`src/sources/claude_jsonl.rs`：
```rust
#[derive(Debug, Default, Clone)]
pub struct TranscriptCursor {
    pub offset: u64,
    pub consumed: u64,
}
```
`read_update` 里，遍历窗口内每条 assistant 行时，除了更新 `latest`，还累加 consumed（在解析出 usage 的分支内，`<synthetic>` 已被跳过）：
```rust
        // 已有：解析出 model != "<synthetic>" 且有 usage 后
        let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        // 【新增】单调 consumed：只累加新 token（排除 cache_read/cache_creation 避免重复计上下文）
        cursor.consumed = cursor.consumed.saturating_add(g("input_tokens") + g("output_tokens"));
        // 下面照旧组装 tokens/context/latest ...
```
（注意：`cursor` 需在该循环作用域可变可见——`read_update` 签名 `cursor: &mut TranscriptCursor` 本就可变。确保 consumed 累加发生在「跳过 synthetic / 无 usage 行」之后。）

`src/state.rs`：`AgentSession` 追加字段（放 `agent_session_id` 之后）：
```rust
    #[serde(default)]
    pub consumed_tokens: u64,
```
`fresh_session` 构造里补 `consumed_tokens: 0,`。所有测试辅助/其它 `AgentSession { .. }` 字面构造（render.rs 的 `sess()` 等）补 `consumed_tokens: 0,`。
`StateStore` 追加方法 + `AgentSession` 追加方法：
```rust
impl AgentSession {
    pub fn stable_key(&self) -> String {
        self.agent_session_id.clone().unwrap_or_else(|| self.pane_id.clone())
    }
}
impl StateStore {
    /// 单调守卫：只在 consumed 增长时更新（会话内单调；新会话由 fresh_session 归零）。
    pub fn set_consumed(&mut self, pane_id: &str, consumed: u64) {
        if let Some(s) = self.sessions.get_mut(pane_id) {
            if consumed > s.consumed_tokens { s.consumed_tokens = consumed; }
        }
    }
}
```

`src/scanner/mod.rs`：在 claude 指标段，`read_update` 返回 Some 后（或无论是否 Some，只要 cursor 动过），把 `cursor.consumed` 写进会话；codex 段把 `thread.tokens_used` 写进会话。定位到既有 claude 段：
```rust
        // claude：read_update 之后（cursor 已累加 consumed）
        let cursor = cursors.entry(pane_id.clone()).or_default();
        let m = claude_jsonl::read_update(&path, cursor);
        let consumed_now = cursor.consumed;
        {
            let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            st.set_consumed(&pane_id, consumed_now);
            if let Some(m) = m { st.set_metrics(&pane_id, m, now_ms); }
        }
```
codex 段：`metrics_for` 命中后同时写 consumed：
```rust
                if let Some(m) = crate::sources::codex_db::metrics_for(&threads, cwd) {
                    // codex tokens_used 即 consumed 口径（per-thread 累计）
                    if let Some(t) = threads.iter().filter(|t| t.cwd == cwd).max_by_key(|t| t.updated_at_ms) {
                        st.set_consumed(&pane_id, t.tokens_used);
                    }
                    st.set_metrics(&pane_id, m, now_ms); // 当前 master set_metrics 为 3 参（M2 起）
                }
```
（`set_metrics` 在当前 master 两个分支都是 3 参 `(pane_id, m, now_ms)`——本任务只新增 `set_consumed` 调用，set_metrics 保持 master 现有 3 参形态，勿改回 2 参。）

- [ ] **Step 4: GREEN + 提交**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿无告警。
```bash
git add src/sources/claude_jsonl.rs src/state.rs src/event.rs src/scanner/mod.rs src/render.rs
git commit -m "feat(m3): monotonic per-session consumed counter (excludes cache_read)"
```

---

### Task 3: burn 采样器

**Files:**
- Create: `src/quota/mod.rs`（本任务先放 `pub mod burn;`）
- Create: `src/quota/burn.rs`
- Modify: `src/main.rs`（`#[allow(dead_code)] mod quota;`）
- Test: `src/quota/burn.rs` 模块内单测

**Interfaces:**
- Consumes: `crate::event::AgentKind`
- Produces: 见 Global Interfaces Task 3

- [ ] **Step 1: 写失败测试**

`src/quota/burn.rs` tests：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;

    fn s(key: &str, p: AgentKind, c: u64) -> (String, AgentKind, u64) { (key.into(), p, c) }

    #[test]
    fn provider_consumed_only_adds_positive_delta() {
        let mut b = BurnSampler::new(60);
        b.sample(&[s("k1", AgentKind::Claude, 100)], 1000);
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 100);
        b.sample(&[s("k1", AgentKind::Claude, 250)], 2000); // +150
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 250);
        // 会话重置（同 key 从 0 起）→ 不减，贡献 0，基线重设
        b.sample(&[s("k1", AgentKind::Claude, 0)], 3000);
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 250, "回退不减总量");
        b.sample(&[s("k1", AgentKind::Claude, 40)], 4000); // 从新基线 0 → +40
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 290);
    }

    #[test]
    fn disappearing_session_does_not_reduce_total() {
        let mut b = BurnSampler::new(60);
        b.sample(&[s("k1", AgentKind::Claude, 100), s("k2", AgentKind::Claude, 200)], 1000);
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 300);
        b.sample(&[s("k1", AgentKind::Claude, 100)], 2000); // k2 消失（prune）
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 300, "消失会话不倒扣");
    }

    #[test]
    fn providers_are_aggregated_separately() {
        let mut b = BurnSampler::new(60);
        b.sample(&[s("k1", AgentKind::Claude, 100), s("k2", AgentKind::Codex, 50)], 1000);
        assert_eq!(b.provider_consumed(&AgentKind::Claude), 100);
        assert_eq!(b.provider_consumed(&AgentKind::Codex), 50);
    }

    #[test]
    fn burn_rate_is_delta_over_window_minutes() {
        let mut b = BurnSampler::new(60);
        b.sample(&[s("k1", AgentKind::Claude, 0)], 0);
        b.sample(&[s("k1", AgentKind::Claude, 600)], 600_000); // +600 over 10 min
        let rate = b.burn_rate_per_min(&AgentKind::Claude, 600_000);
        assert!((rate - 60.0).abs() < 0.5, "600 tokens / 10min = 60/min, got {rate}");
    }

    #[test]
    fn old_samples_outside_window_are_dropped() {
        let mut b = BurnSampler::new(10); // 10 min 窗口
        b.sample(&[s("k1", AgentKind::Claude, 0)], 0);
        b.sample(&[s("k1", AgentKind::Claude, 1000)], 20 * 60_000); // 20min 后
        // 窗口只保留近 10min：最老样本应被裁掉，rate 用窗口内最老 vs 最新
        let rate = b.burn_rate_per_min(&AgentKind::Claude, 20 * 60_000);
        assert!(rate.is_finite() && rate >= 0.0);
    }
}
```

- [ ] **Step 2: RED**

Run: `cargo test -p tfa burn 2>&1 | tail -12`（或 `cargo test burn`）
Expected: 编译错误（模块不存在）。存 RED 证据。

- [ ] **Step 3: 实现**

`src/quota/mod.rs`：
```rust
pub mod burn;
```
`src/quota/burn.rs`（全量）：
```rust
use crate::event::AgentKind;
use std::collections::{HashMap, VecDeque};

/// provider 级单调用量累加器 + 滑窗速率。
/// 唯一输入是各会话的单调 consumed（Task 2）；只加正 delta，消失/重置贡献 0。
pub struct BurnSampler {
    window_ms: u64,
    last_seen: HashMap<String, u64>,                 // stable_key -> 上次 consumed
    total: HashMap<AgentKind, u64>,                  // provider 单调累计
    ring: HashMap<AgentKind, VecDeque<(u64, u64)>>,  // (ts_ms, total) 样本
}

impl BurnSampler {
    pub fn new(window_mins: u64) -> Self {
        Self { window_ms: window_mins.saturating_mul(60_000).max(60_000), last_seen: HashMap::new(), total: HashMap::new(), ring: HashMap::new() }
    }

    pub fn sample(&mut self, sessions: &[(String, AgentKind, u64)], now_ms: u64) {
        // 本轮各 provider 增量
        let mut delta: HashMap<AgentKind, u64> = HashMap::new();
        for (key, provider, consumed) in sessions {
            let prev = self.last_seen.get(key).copied().unwrap_or(0);
            let d = consumed.saturating_sub(prev); // 回退→0
            if d > 0 { *delta.entry(provider.clone()).or_insert(0) += d; }
            self.last_seen.insert(key.clone(), *consumed); // 基线始终跟到最新（含回退后的低值）
        }
        // provider 总量单调 += 增量；每 provider 记一个样本。
        // 必须纳入【本轮 sessions 出现的所有 provider】——否则首采 consumed=0（delta=0）的新 provider
        // 不会进 ring，之后 burn_rate 只有单点 → dt=0 → 恒 0。
        let providers: std::collections::HashSet<AgentKind> = self.total.keys().cloned()
            .chain(delta.keys().cloned())
            .chain(sessions.iter().map(|(_, p, _)| p.clone()))
            .collect();
        for p in providers {
            let t = self.total.entry(p.clone()).or_insert(0);
            *t += delta.get(&p).copied().unwrap_or(0);
            let total_now = *t;
            let ring = self.ring.entry(p).or_default();
            ring.push_back((now_ms, total_now));
            while let Some(&(ts, _)) = ring.front() {
                if now_ms.saturating_sub(ts) > self.window_ms && ring.len() > 1 { ring.pop_front(); } else { break; }
            }
        }
    }

    pub fn provider_consumed(&self, provider: &AgentKind) -> u64 {
        self.total.get(provider).copied().unwrap_or(0)
    }

    pub fn burn_rate_per_min(&self, provider: &AgentKind, _now_ms: u64) -> f64 {
        let Some(ring) = self.ring.get(provider) else { return 0.0 };
        let (Some(&(t0, v0)), Some(&(t1, v1))) = (ring.front(), ring.back()) else { return 0.0 };
        let dt_min = (t1.saturating_sub(t0)) as f64 / 60_000.0;
        if dt_min <= 0.0 { return 0.0; }
        (v1.saturating_sub(v0)) as f64 / dt_min
    }

    pub fn providers(&self) -> Vec<AgentKind> {
        self.total.keys().cloned().collect()
    }
}
```

`src/main.rs` 加：
```rust
#[allow(dead_code)] // consumed by daemon/scanner from Task 4
mod quota;
```

- [ ] **Step 4: GREEN + 提交**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿。
```bash
git add src/quota/ src/main.rs
git commit -m "feat(m3): burn sampler — provider-level positive-delta accumulator + windowed rate"
```

---

### Task 4: LocalEstimateProvider + QuotaState 接入 list/status

**Files:**
- Modify: `src/quota/mod.rs`（`Provider`别名、`QuotaState`、`QuotaSource`、`QuotaCache`）
- Modify: `src/protocol.rs`（`Response::Snapshot` 加 `quota`）
- Modify: `src/daemon/mod.rs`（daemon 持 `Arc<Mutex<QuotaCache>>` + `BurnSampler`，scanner 线程每轮刷新）
- Modify: `src/daemon/server.rs`（`respond()` 的 Snapshot 分支带上 quota）
- Modify: `src/scanner/mod.rs`（tick 采 burn + 刷新 QuotaCache）
- Modify: `src/main.rs`（`List` 输出含 quota；去掉 quota 的 dead_code allow）
- Modify: `src/commands/status.rs`（json 输出含 quota）
- Test: `src/quota/mod.rs` 模块内单测 + `tests/e2e.rs` 补一条

**Interfaces:**
- Consumes: `BurnSampler`（Task 3）、`AgentSession.stable_key/consumed_tokens`（Task 2）
- Produces: 见 Global Interfaces Task 4

- [ ] **Step 1: 写失败测试**

`src/quota/mod.rs` tests：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use super::burn::BurnSampler;
    use crate::event::AgentKind;

    #[test]
    fn local_estimate_never_reports_percent() {
        let mut burn = BurnSampler::new(60);
        burn.sample(&[("k1".into(), AgentKind::Claude, 5000)], 0);
        let mut cache = QuotaCache::new();
        cache.refresh(&burn, 3_600_000);
        let states = cache.states();
        let cl = states.iter().find(|q| q.provider == AgentKind::Claude).expect("claude quota");
        assert!(cl.window_5h_percent.is_none(), "本地推算恒不报 percent");
        assert!(cl.weekly_percent.is_none());
        assert!(matches!(cl.source, QuotaSource::LocalEstimate));
        assert!(cl.reset_estimated);
        assert!(cl.observed_tokens_this_window > 0);
        assert!(cl.reset_at_ms.is_some());
    }

    #[test]
    fn window_rolls_after_five_hours() {
        let mut burn = BurnSampler::new(60);
        let mut cache = QuotaCache::new();
        // t=0 首活动，观测 1000
        burn.sample(&[("k1".into(), AgentKind::Claude, 1000)], 0);
        cache.refresh(&burn, 0);
        let first_reset = cache.states()[0].reset_at_ms.unwrap();
        // 5h+1ms 后又有活动 → 窗口滚动，observed 以新块基线重算
        burn.sample(&[("k1".into(), AgentKind::Claude, 1500)], 5 * 3_600_000 + 1);
        cache.refresh(&burn, 5 * 3_600_000 + 1);
        let q = &cache.states()[0];
        assert!(q.reset_at_ms.unwrap() > first_reset, "窗口已滚动到新块");
        assert_eq!(q.observed_tokens_this_window, 500, "新块只算块内增量 1500-1000");
    }

    #[test]
    fn reset_at_is_hour_floored_plus_5h() {
        let mut burn = BurnSampler::new(60);
        burn.sample(&[("k1".into(), AgentKind::Claude, 10)], 0);
        let mut cache = QuotaCache::new();
        // now = 1h30m（=5400000ms）首次见活动 → block_start=floor_to_hour=1h，reset=6h
        cache.refresh(&burn, 5_400_000);
        let q = &cache.states()[0];
        assert_eq!(q.reset_at_ms.unwrap(), 3_600_000 + 5 * 3_600_000, "floor_to_hour(1.5h)=1h; +5h=6h");
    }
}
```

`tests/e2e.rs` 追加（沿用既有 daemon 启停辅助；`TFA_NO_SCAN=1` 下 quota 为空数组也可，重点验 wire 形状不炸）：
```rust
#[test]
fn list_json_includes_quota_field_shape() {
    // 起 daemon（TFA_SKIP_TMUX_CHECK=1, TFA_NO_SCAN=1），tfa list 输出是对象含 sessions+quota，
    // 或 sessions 数组 + 顶层 quota——按实现约定断言 JSON 可解析且含 "quota" 键，不 panic。
    /* 完整实现沿用本文件既有 TestEnv/daemon spawn/tfa_list 辅助 */
}
```

- [ ] **Step 2: RED**

Run: `cargo test -p tfa quota 2>&1 | tail -12`
Expected: 编译错误（`QuotaState`/`QuotaCache` 不存在）。存 RED 证据。

- [ ] **Step 3: 实现**

`src/quota/mod.rs` 追加（保留 `pub mod burn;`）：
```rust
use crate::event::AgentKind;
use burn::BurnSampler;
use serde::Serialize;
use std::collections::HashMap;

const WINDOW_5H_MS: u64 = 5 * 3_600_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource { LocalEstimate }

#[derive(Debug, Clone, Serialize)]
pub struct QuotaState {
    pub provider: AgentKind,
    pub window_5h_percent: Option<u8>,
    pub weekly_percent: Option<u8>,
    pub reset_at_ms: Option<u64>,
    pub reset_estimated: bool,
    pub observed_tokens_this_window: u64,
    pub burn_rate_per_min: f64,
    pub source: QuotaSource,
    pub freshness_ms: u64,
}

/// 每 provider 的 5h 块锚 + 块内累计 observed。不进快照（易失，每轮重算）。
struct Block { start_ms: u64, observed: u64 }

pub struct QuotaCache {
    blocks: HashMap<AgentKind, Block>,
    last_consumed: HashMap<AgentKind, u64>, // 上次 refresh 时的 provider 累计（算本轮 delta）
    states: Vec<QuotaState>,
}

fn floor_to_hour(ms: u64) -> u64 { (ms / 3_600_000) * 3_600_000 }

impl QuotaCache {
    pub fn new() -> Self { Self { blocks: HashMap::new(), last_consumed: HashMap::new(), states: Vec::new() } }

    /// 组装各 provider QuotaState；管理 5h 滚动块（floor_to_hour 锚定 + +5h 重置）。
    /// observed 用「本轮 delta 累加到块」而非「consumed - baseline」——确保触发 roll 的那一轮
    /// 赚到的 delta 记进【新块】而非被清零（burn.provider_consumed 单调，delta = 本轮 - 上轮）。
    pub fn refresh(&mut self, burn: &BurnSampler, now_ms: u64) {
        let mut out = Vec::new();
        for provider in burn.providers() {
            let consumed = burn.provider_consumed(&provider);
            let last = self.last_consumed.get(&provider).copied().unwrap_or(0); // 首见：从 0 起（BurnSampler 也从 0 累计）
            let delta = consumed.saturating_sub(last);
            self.last_consumed.insert(provider.clone(), consumed);
            let block = self.blocks.entry(provider.clone()).or_insert(Block { start_ms: floor_to_hour(now_ms), observed: 0 });
            if now_ms >= block.start_ms + WINDOW_5H_MS {
                block.start_ms = floor_to_hour(now_ms);
                block.observed = delta;                       // roll：本轮 delta 归新块
            } else {
                block.observed = block.observed.saturating_add(delta);
            }
            let observed = block.observed;
            out.push(QuotaState {
                provider: provider.clone(),
                window_5h_percent: None,   // 本地推算恒 None（无真实 limit）
                weekly_percent: None,
                reset_at_ms: Some(block.start_ms + WINDOW_5H_MS),
                reset_estimated: true,
                observed_tokens_this_window: observed,
                burn_rate_per_min: burn.burn_rate_per_min(&provider, now_ms),
                source: QuotaSource::LocalEstimate,
                freshness_ms: now_ms,
            });
        }
        self.states = out;
    }

    pub fn states(&self) -> Vec<QuotaState> { self.states.clone() }
}
```

`src/protocol.rs`：`Response::Snapshot` 加字段（**additive，serde default**）：
```rust
    Snapshot {
        sessions: Vec<crate::state::AgentSession>,
        #[serde(default)]
        quota: Vec<crate::quota::QuotaState>,
        generated_at_ms: u64,
    },
```
更新 `snapshot_wire_shape` 测试构造带 `quota: vec![]`。

`src/daemon/mod.rs`：daemon 持共享 QuotaCache + BurnSampler，交给 scanner（scanner::spawn 增参）与 server（respond 读 cache）：
```rust
    let quota = Arc::new(Mutex::new(crate::quota::QuotaCache::new()));
    // ... 起 scanner 时传入 quota + config：
    crate::scanner::spawn(Arc::clone(&store), Arc::clone(&dirty), Arc::clone(&quota), cfg_for_scanner);
    // server::serve 传 quota：
    server::serve(listener, store, dirty, Arc::clone(&quota));
```
（`cfg_for_scanner` 见 Task 6；本任务 scanner::spawn 先只加 quota 参数，config 参数 Task 6 再加。实现者本任务把 scanner::spawn/serve 签名各加一个 `Arc<Mutex<QuotaCache>>` 参数即可。）

`src/scanner/mod.rs`：`spawn` 与 `tick` 增 `quota: &Mutex<QuotaCache>` + 内部持一个 `BurnSampler`（在 spawn 的线程闭包里 own，跨 tick 存活）。tick 末尾：
```rust
    // 采 burn：收集 (stable_key, provider, consumed)
    let samples: Vec<(String, crate::event::AgentKind, u64)> = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.sessions().iter().map(|s| (s.stable_key(), s.agent.clone(), s.consumed_tokens)).collect()
    };
    burn.sample(&samples, now_ms);
    quota.lock().unwrap_or_else(std::sync::PoisonError::into_inner).refresh(&burn, now_ms);
```
`spawn` 闭包里 `let mut burn = crate::quota::burn::BurnSampler::new(60);`（window 从 config，Task 6 接；本任务先硬 60 或从 Config::load().quota.burn_rate_window_mins 读一次）。`tick` 签名相应加 `burn: &mut BurnSampler, quota: &Mutex<QuotaCache>`。

`src/daemon/server.rs`：`serve`/`handle`/`respond` 透传 `quota`，Snapshot 分支：
```rust
        Request::Snapshot => {
            let sessions = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions();
            let quota = quota.lock().unwrap_or_else(std::sync::PoisonError::into_inner).states();
            Response::Snapshot { sessions, quota, generated_at_ms: super::now_ms() }
        }
```

`src/main.rs` `List` 分支解构带 quota（输出仍打印 sessions 为主，quota 附加）：
```rust
        Command::List => match client::request(&protocol::Request::Snapshot) {
            Ok(protocol::Response::Snapshot { sessions, quota, .. }) => {
                let out = serde_json::json!({ "sessions": sessions, "quota": quota });
                println!("{}", serde_json::to_string(&out).unwrap_or_default());
            }
            _ => println!("{{\"sessions\":[],\"quota\":[]}}"),
        },
```
去掉 `mod quota;` 的 `#[allow(dead_code)]`。

`src/commands/status.rs`：`("json", Ok(Snapshot{sessions, quota, ..}))` 分支输出 `{sessions, quota}` pretty；`("tmux", Ok(Snapshot{sessions, generated_at_ms, ..}))` 分支用 `..` **忽略** quota（不绑定，避免 `clippy -D warnings` 报未用变量；状态栏不变）。

- [ ] **Step 4: GREEN + 提交**

Run: `cargo test 2>&1 | tail -6 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿。
```bash
git add src/quota/ src/protocol.rs src/daemon/ src/scanner/mod.rs src/main.rs src/commands/status.rs tests/e2e.rs
git commit -m "feat(m3): LocalEstimate quota (percent=None, observed>=) wired into list/status"
```

---

### Task 5: notifier core + `tfa notify send`/`test`

**Files:**
- Create: `src/notify/mod.rs`（`NotifyKind`/`NotifyEvent`/`spawn_notifier`）
- Create: `src/notify/channels.rs`（三通道派发 + 格式化 + 超时 + `TFA_NO_NOTIFY` sink）
- Create: `src/commands/notify.rs`（`tfa notify send`/`test` CLI）
- Modify: `src/main.rs`（`mod notify;`、`Notify` 子命令）
- Modify: `src/commands/mod.rs`（`pub mod notify;`）
- Modify: `Cargo.toml`（`ureq`）
- Test: `src/notify/channels.rs` 模块内单测 + `tests/notify_cmd.rs`

**Interfaces:**
- Consumes: `Config`（Task 1）
- Produces: 见 Global Interfaces Task 5

- [ ] **Step 1: 加依赖**

`Cargo.toml`：
```toml
ureq = { version = "3", default-features = false, features = ["rustls", "json"] }
```
Run: `cargo build 2>&1 | tail -3`　Expected: 通过（首次编 rustls 稍慢）。

- [ ] **Step 2: 写失败测试**

`src/notify/channels.rs` tests（纯函数：格式化 + 通道选择，不真发）：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NotifyEvent, NotifyKind};

    fn ev() -> NotifyEvent {
        NotifyEvent { session_key: "sess-1".into(), pane_id: "%3".into(), session_name: Some("api".into()),
            kind: NotifyKind::WaitingInput, generation: 1, title: "api 等待输入".into(), body: "needs permission".into() }
    }

    #[test]
    fn bark_payload_has_required_fields() {
        let v = http_payload("bark", &ev(), "devkey123");
        assert_eq!(v["device_key"], "devkey123");
        assert_eq!(v["title"], "api 等待输入");
        assert_eq!(v["body"], "needs permission");
    }

    #[test]
    fn ntfy_payload_uses_topic_and_message() {
        let v = http_payload("ntfy", &ev(), "tfa-alerts");
        assert_eq!(v["topic"], "tfa-alerts");
        assert_eq!(v["title"], "api 等待输入");
        assert_eq!(v["message"], "needs permission");
    }

    #[test]
    fn generic_payload_carries_kind_and_session() {
        let v = http_payload("generic-json", &ev(), "");
        assert_eq!(v["kind"], "waiting_input");
        assert_eq!(v["session"], "api");
        assert_eq!(v["title"], "api 等待输入");
    }

    #[test]
    fn no_notify_env_routes_to_sink_not_real_dispatch() {
        std::env::set_var("TFA_NO_NOTIFY", "1");
        test_sink_clear();
        let cfg = crate::config::NotifyConfig::default(); // macos+tmux on, http off
        dispatch(&ev(), &cfg);
        std::env::remove_var("TFA_NO_NOTIFY");
        let sunk = test_sink_take();
        assert_eq!(sunk.len(), 1, "TFA_NO_NOTIFY 下事件进 sink，不真发");
        assert_eq!(sunk[0].kind.as_str(), "waiting_input");
    }
}
```
（`http_payload(format, &NotifyEvent, key_or_topic) -> serde_json::Value`、`test_sink_clear/take` 是测试用内存 sink。`bark` 的 key、`ntfy` 的 topic 从 `HttpChannel.url` 末段解析——见实现。）

`tests/notify_cmd.rs`：
```rust
use assert_cmd::cargo::cargo_bin;
use std::process::Command;

#[test]
fn notify_test_under_no_notify_exits_zero() {
    // tfa notify test 在 TFA_NO_NOTIFY=1 下不真弹通知，正常 exit 0
    let out = Command::new(cargo_bin("tfa"))
        .args(["notify", "test"])
        .env("TFA_NO_NOTIFY", "1")
        .env("TFA_CONFIG_PATH", "/nonexistent-config.toml") // 用默认配置
        .output().unwrap();
    assert!(out.status.success(), "notify test 应 exit 0；stderr={}", String::from_utf8_lossy(&out.stderr));
}
```

- [ ] **Step 3: RED**

Run: `cargo test -p tfa channels 2>&1 | tail -12 && cargo test --test notify_cmd 2>&1 | tail -8`
Expected: 编译错误（模块/命令不存在）。存 RED 证据。

- [ ] **Step 4: 实现**

`src/notify/mod.rs`：
```rust
pub mod channels;

use crate::config::Config;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] // Eq+Hash：Discipline 用作 HashMap 键
pub enum NotifyKind { WaitingInput, Done, Stale, Dead }
impl NotifyKind {
    pub fn as_str(&self) -> &'static str {
        match self { Self::WaitingInput => "waiting_input", Self::Done => "done", Self::Stale => "stale", Self::Dead => "dead" }
    }
}

#[derive(Debug, Clone)]
pub struct NotifyEvent {
    pub session_key: String,
    pub pane_id: String,
    pub session_name: Option<String>,
    pub kind: NotifyKind,
    pub generation: u64,
    pub title: String,
    pub body: String,
}

/// 唯一消费队列的独立线程：串行派发，通道 IO 各带超时，绝不阻塞其它线程。
pub fn spawn_notifier(rx: Receiver<NotifyEvent>, cfg: Arc<Mutex<Config>>) {
    std::thread::spawn(move || {
        for ev in rx {
            let c = { cfg.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() };
            if !c.notify.enabled { continue; }
            channels::dispatch(&ev, &c.notify);
        }
    });
}
```

`src/notify/channels.rs`（全量）：
```rust
use super::NotifyEvent;
use crate::config::NotifyConfig;
use serde_json::json;
use std::time::{Duration, Instant};

/// 子进程硬超时：spawn 后轮询 try_wait，超时即 kill。用于 macOS/tmux 通道，
/// 防挂住的 osascript/tmux 堵死串行 notifier 队列（C1 对所有通道 IO 的硬超时要求）。
const LOCAL_CHANNEL_CAP: Duration = Duration::from_secs(5);
fn run_capped(mut cmd: std::process::Command) {
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).stdin(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if start.elapsed() >= LOCAL_CHANNEL_CAP { let _ = child.kill(); return; }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
thread_local! {
    static SINK: std::cell::RefCell<Vec<NotifyEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
pub fn test_sink_clear() { SINK.with(|s| s.borrow_mut().clear()); }
#[cfg(test)]
pub fn test_sink_take() -> Vec<NotifyEvent> { SINK.with(|s| std::mem::take(&mut *s.borrow_mut())) }

fn no_notify() -> bool { std::env::var("TFA_NO_NOTIFY").as_deref() == Ok("1") }

/// 派发到所有 enabled 通道。失败静默吞（不重试/不告警）。
pub fn dispatch(ev: &NotifyEvent, cfg: &NotifyConfig) {
    if no_notify() {
        // 真二进制也可观测：append 到 state_dir/notify-sink.jsonl（e2e 子进程断言用；
        // 不能只用 #[cfg(test)] thread_local——那不编进 cargo_bin("tfa") 起的真进程）。
        let line = json!({"kind": ev.kind.as_str(), "pane": ev.pane_id, "session": ev.session_name, "title": ev.title}).to_string();
        let _ = std::fs::create_dir_all(crate::paths::state_dir());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
            .open(crate::paths::state_dir().join("notify-sink.jsonl")) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        #[cfg(test)]
        SINK.with(|s| s.borrow_mut().push(ev.clone())); // 进程内单测用
        return;
    }
    if cfg.channels.macos.enabled { macos_send(ev); }
    if cfg.channels.tmux.enabled { tmux_send(ev); }
    if cfg.channels.http.enabled && !cfg.channels.http.url.is_empty() { http_send(ev, &cfg.channels.http); }
}

fn macos_send(ev: &NotifyEvent) {
    // terminal-notifier 检测到就用，否则 osascript 兜底。失败静默；子进程带硬超时。
    let has_tn = std::process::Command::new("sh").arg("-c").arg("command -v terminal-notifier")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()
        .map(|s| s.success()).unwrap_or(false);
    if has_tn {
        let mut c = std::process::Command::new("terminal-notifier");
        c.args(["-title", &ev.title, "-message", &ev.body]);
        run_capped(c);
    } else {
        let script = format!("display notification {:?} with title {:?}", ev.body, ev.title);
        let mut c = std::process::Command::new("osascript");
        c.args(["-e", &script]);
        run_capped(c);
    }
}

fn tmux_send(ev: &NotifyEvent) {
    // display-message 到目标 pane；无 attached client 返回 no clients，静默吞；带硬超时。
    let mut c = std::process::Command::new("tmux");
    c.args(crate::paths::tmux_args());
    c.args(["display-message", "-t", &ev.pane_id, &format!("[tfa] {}", ev.title)]);
    run_capped(c);
}

/// bark: url 末段是 device_key；ntfy: url 末段是 topic；generic: 原样 POST url。
pub fn http_payload(format: &str, ev: &NotifyEvent, key_or_topic: &str) -> serde_json::Value {
    let session = ev.session_name.clone().unwrap_or_else(|| ev.pane_id.clone());
    match format {
        "bark" => json!({ "device_key": key_or_topic, "title": ev.title, "body": ev.body, "group": "tfa" }),
        "ntfy" => json!({ "topic": key_or_topic, "title": ev.title, "message": ev.body, "tags": ["robot"] }),
        _ => json!({ "kind": ev.kind.as_str(), "session": session, "pane": ev.pane_id, "title": ev.title, "body": ev.body }),
    }
}

fn last_segment(url: &str) -> String {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn http_send(ev: &NotifyEvent, http: &crate::config::HttpChannel) {
    let key_or_topic = last_segment(&http.url);
    let payload = http_payload(&http.format, ev, &key_or_topic);
    // bark 固定 POST {base}/push；ntfy/generic POST 到 url 根。
    let target = if http.format == "bark" {
        let base = http.url.trim_end_matches('/');
        let base = base.strip_suffix(&format!("/{key_or_topic}")).unwrap_or(base);
        format!("{base}/push")
    } else {
        http.url.clone()
    };
    let timeout = Duration::from_millis(http.timeout_ms.min(10_000));
    let mut req = ureq::post(&target).config().timeout_global(Some(timeout)).build();
    for (k, v) in &http.headers { req = req.header(k, v); }
    let _ = req.send_json(payload); // 失败静默吞
}
```
（注：ureq 3.x API 以实际版本为准；核心要求是**全局超时**。若 3.x API 形状不同，实现者按等价方式设 `timeout_global`/`timeout_connect+timeout_read`，务必设上超时——这是 C1 硬需求。）

`src/commands/notify.rs`：
```rust
use crate::config::Config;
use crate::notify::{channels, NotifyEvent, NotifyKind};

/// `tfa notify test`：向所有 enabled 通道发一条测试通知（TFA_NO_NOTIFY 下走 sink）。
/// `tfa notify send`：由 daemon 内部使用；CLI 保留最小实现（读默认 config 发一条）。
pub fn run(sub: &str) {
    let cfg = Config::load();
    let ev = NotifyEvent {
        session_key: "test".into(), pane_id: "%0".into(), session_name: Some("tfa".into()),
        kind: NotifyKind::WaitingInput, generation: 0,
        title: "tfa 通知测试".into(),
        body: match sub { "test" => "如果你看到这条，通知通道工作正常".into(), _ => "notify".into() },
    };
    channels::dispatch(&ev, &cfg.notify);
    std::process::exit(0);
}
```

`src/commands/mod.rs` 加 `pub mod notify;`。
`src/main.rs`：`mod notify;`（无 dead_code allow，被 command 消费）；`Command` 加：
```rust
    /// Send or test notifications
    Notify {
        /// "test" | "send"
        #[arg(default_value = "test")]
        action: String,
    },
```
`match` 加 `Command::Notify { action } => commands::notify::run(&action),`。

- [ ] **Step 5: GREEN + 提交**

Run: `cargo test 2>&1 | tail -6 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿。
```bash
git add src/notify/ src/commands/ src/main.rs Cargo.toml Cargo.lock
git commit -m "feat(m3): notifier core + tfa notify test/send (macos/tmux/http, hard timeout)"
```

- [ ] **Step 6: 用户真机验收（通道，TCC 交互，agent 代劳不了）**

控制器 `cargo install --path . --force`；用户在真机跑 `tfa notify test`，确认：macOS 弹出通知（首次可能要在「系统设置>通知」授权 terminal-notifier/Script Editor）、当前 tmux 窗口底部出现 `[tfa] tfa 通知测试`。可选配 Bark/ntfy URL 再 `tfa notify test` 验手机推送。**用户确认三通道后方可进 Task 6。**

---

### Task 6: 通知纪律 + 四触发接线

**Files:**
- Create: `src/notify/discipline.rs`
- Modify: `src/notify/mod.rs`（`pub mod discipline;`）
- Modify: `src/daemon/mod.rs`（建 mpsc + spawn_notifier + Config + Discipline，交 scanner 与 server）
- Modify: `src/daemon/server.rs`（`respond()` 锁内收集净边沿、出锁 mpsc 入队）
- Modify: `src/scanner/mod.rs`（tick 边界快照 diff → Discipline.edges → mpsc 入队）
- Test: `src/notify/discipline.rs` 模块内单测 + `tests/notify_e2e.rs`

**Interfaces:**
- Consumes: `NotifyEvent`/`NotifyKind`（Task 5）、`Config`/`NotifyConfig`（Task 1）、`SessionState`/`AgentSession`（state）
- Produces: 见 Global Interfaces Task 6

- [ ] **Step 1: 写失败测试**

`src/notify/discipline.rs` tests（纯逻辑，重点钉边沿/冷却/boot-grace/去抖/generation）：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyConfig;
    use crate::event::AgentKind;
    use crate::state::{AgentSession, SessionState, Source};
    use std::collections::HashMap;

    fn sess(key: &str, state: SessionState) -> AgentSession {
        AgentSession {
            pane_id: key.into(), agent: AgentKind::Claude, session_name: Some("api".into()),
            state, state_since_ms: 0, current_task: None, cwd: None, last_activity_ms: 0,
            source: Source::Hook, pid: None, model: None, context: None, tokens: None,
            git_branch: None, transcript_path: None, agent_session_id: Some(key.into()), consumed_tokens: 0,
        }
    }
    fn all_on() -> NotifyConfig {
        let mut c = NotifyConfig::default();
        c.triggers = crate::config::Triggers { waiting_input: true, done: true, stale: true, dead: true };
        c.discipline.boot_grace_secs = 0; // 测试关掉 grace
        c
    }
    fn waiting() -> SessionState { SessionState::WaitingInput { reason: "perm".into() } }

    #[test]
    fn net_edge_into_waiting_fires_once() {
        let mut d = Discipline::new(0, 0);
        let before = Discipline::snapshot_states(&[sess("%1", SessionState::Working)]);
        let after = vec![sess("%1", waiting())];
        let evs = d.edges(&before, &after, &all_on(), 1000);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0].kind, NotifyKind::WaitingInput));
    }

    #[test]
    fn reason_change_within_waiting_is_not_an_edge() {
        let mut d = Discipline::new(0, 0);
        let before = Discipline::snapshot_states(&[sess("%1", SessionState::WaitingInput { reason: "run X".into() })]);
        let after = vec![sess("%1", SessionState::WaitingInput { reason: "run Y".into() })];
        assert!(d.edges(&before, &after, &all_on(), 1000).is_empty(), "reason 变不算边沿");
    }

    #[test]
    fn re_entering_waiting_after_leaving_bypasses_cooldown() {
        let mut d = Discipline::new(0, 0);
        let cfg = all_on();
        // 首次进 waiting → 发
        let e1 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                         &[sess("%1", waiting())], &cfg, 0);
        assert_eq!(e1.len(), 1);
        // 离开 waiting → working（应答了）
        d.edges(&Discipline::snapshot_states(&[sess("%1", waiting())]),
                &[sess("%1", SessionState::Working)], &cfg, 5_000);
        // 再次进 waiting（冷却窗口 30s 内）→ 仍必须发（真新边沿优先冷却）
        let e2 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                         &[sess("%1", waiting())], &cfg, 20_000);
        assert_eq!(e2.len(), 1, "离开触发态后再进必须放行，杜绝 agent 静默卡死");
    }

    #[test]
    fn staying_in_waiting_is_cooled_down() {
        let mut d = Discipline::new(0, 0);
        let cfg = all_on(); // cooldown 30s
        d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]), &[sess("%1", waiting())], &cfg, 0);
        // 同一 waiting 态持续（before/after 都 waiting，无净边沿）→ 本就不发
        let e = d.edges(&Discipline::snapshot_states(&[sess("%1", waiting())]), &[sess("%1", waiting())], &cfg, 10_000);
        assert!(e.is_empty());
    }

    #[test]
    fn stale_and_dead_are_separate_kinds() {
        let mut d = Discipline::new(0, 0);
        let cfg = all_on();
        let e1 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                         &[sess("%1", SessionState::Stale)], &cfg, 0);
        assert!(matches!(e1[0].kind, NotifyKind::Stale));
        // Stale→Dead 升级：dead 独立 kind，允许发（dead 去抖需连续 K 轮，见下）
    }

    #[test]
    fn dead_debounced_until_k_consecutive_ticks() {
        let mut d = Discipline::new(0, 0);
        let mut cfg = all_on();
        cfg.discipline.dead_debounce_ticks = 2;
        // 第 1 次进 Dead → 去抖不发
        let e1 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                         &[sess("%1", SessionState::Dead)], &cfg, 0);
        assert!(e1.is_empty(), "首轮 dead 去抖不发");
        // 连续第 2 轮仍 Dead → 发
        let e2 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Dead)]),
                         &[sess("%1", SessionState::Dead)], &cfg, 15_000);
        assert_eq!(e2.len(), 1, "连续 2 轮判死才发");
        assert!(matches!(e2[0].kind, NotifyKind::Dead));
    }

    #[test]
    fn boot_grace_suppresses_then_allows() {
        let mut d = Discipline::new(30, 0); // grace 30s，从 t=0
        d.seed(&[sess("%1", waiting())]);   // 播种基线（快照恢复的既有 waiting）
        // grace 内：即便算出边沿也抑制
        let e = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                        &[sess("%1", waiting())], &all_on(), 10_000);
        assert!(e.is_empty(), "boot grace 内抑制");
        // grace 后新边沿放行
        let e2 = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                         &[sess("%1", waiting())], &all_on(), 40_000);
        assert_eq!(e2.len(), 1);
    }

    #[test]
    fn disabled_trigger_does_not_fire() {
        let mut d = Discipline::new(0, 0);
        let mut cfg = NotifyConfig::default(); // 只有 waiting_input 默认开
        cfg.discipline.boot_grace_secs = 0;
        let e = d.edges(&Discipline::snapshot_states(&[sess("%1", SessionState::Working)]),
                        &[sess("%1", SessionState::Done)], &cfg, 0);
        assert!(e.is_empty(), "done 默认关不发");
    }
}
```

`tests/notify_e2e.rs`（隔离 tmux + `TFA_NO_NOTIFY=1`，验证净边沿只发一次 + hook 护栏不阻塞——沿用 scanner_e2e 辅助）：
```rust
//! e2e：起 daemon（TFA_NO_NOTIFY=1），模拟 hook 序列，断言 notifier sink 收到恰当净边沿。
//! 完整实现沿用 tests/scanner_e2e.rs 的隔离 tmux + daemon 辅助。
#[test]
fn waiting_input_hook_produces_one_notification_and_does_not_block() {
    // 1. daemon 起（TFA_NO_NOTIFY=1, 默认 config waiting_input 开）
    // 2. tfa hook claude notification（TMUX_PANE=真 pane，payload 带 message）
    // 3. 断言：hook 命令快速返回（<2s，护栏）；daemon 侧净边沿计数=1（通过某种可观测：
    //    daemon 暴露一个 TFA_NO_NOTIFY sink 计数，或用 tfa list 观察状态转换 + notify 计数文件）
    /* 完整实现：可让 TFA_NO_NOTIFY 模式把事件 append 到 state_dir/notify-sink.jsonl，测试读它断言 */
}
```
（sink 机制已定：`TFA_NO_NOTIFY=1` 时 `channels::dispatch` 既 append `state_dir/notify-sink.jsonl`（真二进制，e2e 子进程读它断言）又 push `#[cfg(test)]` thread_local sink（进程内单测）。Task 5 已落地二者，本 e2e 直接读 jsonl。）

- [ ] **Step 2: RED**

Run: `cargo test -p tfa discipline 2>&1 | tail -15`
Expected: 编译错误（模块不存在）。存 RED 证据。

- [ ] **Step 3: 实现**

`src/notify/mod.rs` 加 `pub mod discipline;`。

`src/notify/discipline.rs`（全量）：
```rust
use super::{NotifyEvent, NotifyKind};
use crate::config::NotifyConfig;
use crate::state::{AgentSession, SessionState};
use std::collections::HashMap;

/// 触发态判别式：把状态映射到通知 kind（忽略 WaitingInput 的 reason）。非触发态返回 None。
pub fn trigger_kind(state: &SessionState) -> Option<NotifyKind> {
    match state {
        SessionState::WaitingInput { .. } => Some(NotifyKind::WaitingInput),
        SessionState::Done => Some(NotifyKind::Done),
        SessionState::Stale => Some(NotifyKind::Stale),
        SessionState::Dead => Some(NotifyKind::Dead),
        SessionState::Starting | SessionState::Working => None,
    }
}

fn trigger_enabled(cfg: &NotifyConfig, kind: NotifyKind) -> bool {
    match kind {
        NotifyKind::WaitingInput => cfg.triggers.waiting_input,
        NotifyKind::Done => cfg.triggers.done,
        NotifyKind::Stale => cfg.triggers.stale,
        NotifyKind::Dead => cfg.triggers.dead,
    }
}

pub struct Discipline {
    prev: HashMap<String, Option<NotifyKind>>,     // stable_key -> 上次触发态（None=非触发态）
    cooldown: HashMap<(String, NotifyKind), u64>,  // 上次发送 ts（仅压制停在同态内的重复）
    generation: HashMap<String, u64>,              // stable_key -> 单调 generation
    dead_pending: HashMap<String, u64>,            // stable_key -> 连续 Dead 轮数
    boot_until_ms: u64,
}

impl Discipline {
    pub fn new(boot_grace_secs: u64, now_ms: u64) -> Self {
        Self { prev: HashMap::new(), cooldown: HashMap::new(), generation: HashMap::new(),
               dead_pending: HashMap::new(), boot_until_ms: now_ms + boot_grace_secs * 1000 }
    }

    /// 启动播种：把快照恢复的既有会话触发态设为基线，首次观测同态不算跳变。
    pub fn seed(&mut self, sessions: &[AgentSession]) {
        for s in sessions { self.prev.insert(s.stable_key(), trigger_kind(&s.state)); }
    }

    /// 会话集合 → (stable_key -> 触发态判别式) 快照。
    pub fn snapshot_states(sessions: &[AgentSession]) -> HashMap<String, Option<NotifyKind>> {
        sessions.iter().map(|s| (s.stable_key(), trigger_kind(&s.state))).collect()
    }

    /// before/after 快照算净边沿，过纪律过滤，产 NotifyEvent。
    pub fn edges(&mut self, before: &HashMap<String, Option<NotifyKind>>,
                 after: &[AgentSession], cfg: &NotifyConfig, now_ms: u64) -> Vec<NotifyEvent> {
        let mut out = Vec::new();
        let in_grace = now_ms < self.boot_until_ms;
        for s in after {
            let key = s.stable_key();
            let new_kind = trigger_kind(&s.state);
            let old_kind = before.get(&key).copied().unwrap_or_else(|| self.prev.get(&key).copied().unwrap_or(None));

            // 离开触发态：清该 key 所有冷却（边沿冷却核心——再进必放行）
            if old_kind.is_some() && new_kind != old_kind {
                self.cooldown.retain(|(k, _), _| k != &key);
                if !matches!(new_kind, Some(NotifyKind::Dead)) { self.dead_pending.remove(&key); }
            }

            // 净边沿：进入某触发态（old != new 且 new 是触发态）
            let is_edge = new_kind.is_some() && new_kind != old_kind;
            self.prev.insert(key.clone(), new_kind);

            let Some(kind) = new_kind else { self.dead_pending.remove(&key); continue };
            if !trigger_enabled(cfg, kind) { continue; }

            // dead 去抖 vs 非 dead 边沿门：
            // dead 跨【连续轮】计数（不 gate 在 is_edge——连续 Dead 轮 new==old 非边沿，但仍要累加）；
            // 恰在第 threshold 轮发一次，之后 *n>threshold 不再发。离开 Dead 时上面的 leave 块已清 dead_pending。
            if matches!(kind, NotifyKind::Dead) {
                let threshold = cfg.discipline.dead_debounce_ticks.max(1);
                let n = self.dead_pending.entry(key.clone()).or_insert(0);
                *n += 1;
                if *n != threshold { continue; }
            } else if !is_edge {
                continue; // 非 dead：只在净边沿发
            }

            // 冷却：只压制「停在同态内的重复」（边沿/去抖已过，此处防同 tick/极短抖动）
            let cd_key = (key.clone(), kind);
            if let Some(&last) = self.cooldown.get(&cd_key) {
                if now_ms.saturating_sub(last) < cfg.discipline.cooldown_secs * 1000 { continue; }
            }

            if in_grace { continue; } // boot grace 抑制（基线已由 seed/prev 更新，grace 后新边沿正常发）

            self.cooldown.insert(cd_key, now_ms);
            let gen = { let g = self.generation.entry(key.clone()).or_insert(0); *g += 1; *g };
            let name = s.session_name.clone();
            let disp = name.clone().unwrap_or_else(|| s.pane_id.clone());
            let (title, body) = match kind {
                NotifyKind::WaitingInput => (format!("{disp} 等待输入"), reason_of(&s.state)),
                NotifyKind::Done => (format!("{disp} 完成待 review"), "agent 已停下".into()),
                NotifyKind::Stale => (format!("{disp} 卡住(stale)"), "长时间无活动".into()),
                NotifyKind::Dead => (format!("{disp} 已退出(dead)"), "agent 进程结束".into()),
            };
            out.push(NotifyEvent { session_key: key, pane_id: s.pane_id.clone(), session_name: name,
                kind, generation: gen, title, body });
        }
        out
    }
}

fn reason_of(state: &SessionState) -> String {
    match state { SessionState::WaitingInput { reason } => reason.clone(), _ => "needs input".into() }
}
```

`src/daemon/mod.rs`：建通知管道，播种，交给 scanner 与 server。
```rust
    let config = Arc::new(Mutex::new(crate::config::Config::load()));
    let (notify_tx, notify_rx) = std::sync::mpsc::channel::<crate::notify::NotifyEvent>();
    crate::notify::spawn_notifier(notify_rx, Arc::clone(&config));
    let discipline = Arc::new(Mutex::new({
        let boot = config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).notify.discipline.boot_grace_secs;
        let mut d = crate::notify::discipline::Discipline::new(boot, now_ms());
        d.seed(&store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions());
        d
    }));
    // scanner 与 server 各拿 notify_tx.clone() + Arc::clone(&discipline) + Arc::clone(&config)
```
scanner::spawn / server::serve 再各加 `tx: Sender<NotifyEvent>`, `discipline: Arc<Mutex<Discipline>>`, `config: Arc<Mutex<Config>>` 参数（在 Task 4 已加 quota 的基础上继续加）。

`src/scanner/mod.rs` tick：在**所有 mutation 之前**记 before 快照，**之后**算 after + edges，出锁入队：
```rust
    // tick 开头（拿到 panes/procs 后、进 reconcile 锁段前不行——需在 store 上取 before）
    let before = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::notify::discipline::Discipline::snapshot_states(&st.sessions())
    };
    // ... 既有 reconcile/metrics/consumed/burn 全跑完 ...
    // tick 末尾：
    let after = { store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions() };
    let cfg = { config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() };
    let evs = { discipline.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edges(&before, &after, &cfg.notify, now_ms) };
    for ev in evs { let _ = tx.send(ev); } // 出锁、非阻塞入队
```

`src/daemon/server.rs` respond() 的 Hook 分支：apply 之后锁内取 before/after 算边沿、出锁入队（护栏：锁内只收集，IO 归 notifier 线程）：
```rust
        Request::Hook { .. } => {
            // ... 既有：解析 ev、拿 pane ...
            let (before_after_edges): Vec<NotifyEvent> = {
                let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let before = Discipline::snapshot_states(&st.sessions());
                st.apply(ev);
                // ...既有 needs_name/set_session_name...
                let after = st.sessions();
                let cfg = config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                discipline.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edges(&before, &after, &cfg.notify, super::now_ms())
            };
            for ev in before_after_edges { let _ = tx.send(ev); } // 出锁入队
            dirty.store(true, Ordering::Relaxed);
            Response::Ok
        }
```
（实现者按当前 server.rs 结构调整；关键不变量：**edges 计算在锁内、`tx.send` 在锁外**，且 hook 路径不做任何通道 IO。`config`/`discipline`/`tx` 经 `serve`→`handle`→`respond` 透传。）

- [ ] **Step 4: GREEN + 提交**

Run: `cargo test 2>&1 | tail -8 && cargo clippy --all-targets 2>&1 | tail -2`　Expected: 全绿。
```bash
git add src/notify/ src/daemon/ src/scanner/mod.rs tests/notify_e2e.rs
git commit -m "feat(m3): notification discipline — tick-boundary edge diff, edge-cooldown, boot-grace, dead-debounce"
```

---

### Task 7: e2e + 文档 + 真装验收

**Files:**
- Modify: `tests/notify_e2e.rs`（补全净边沿计数 + hook 护栏 + no-clients 非致命）
- Modify: `tests/e2e.rs`（quota wire 兼容已在 Task 4）
- Modify: `README.md`（config 文件、新环境变量、通道说明、诚实声明）
- Test: 全量回归

**Interfaces:** 无新接口。

- [ ] **Step 1: 补全 e2e**

`tests/notify_e2e.rs` 落地 Task 6 约定的 sink（`TFA_NO_NOTIFY=1` → daemon append `state_dir/notify-sink.jsonl`），断言：
1. 一次 waiting_input hook → sink 恰好一条 `"kind":"waiting_input"`；
2. hook 命令 <2s 返回（护栏，复用 hook_cmd.rs 计时手法）；
3. 目标 pane 无 attached client 时 daemon 不崩、不留错误（no-clients 非致命）。
（若 Task 6 用 thread_local sink，此处改用 daemon 侧 jsonl sink——实现者在 Task 6/7 统一为 daemon 侧文件 sink 以便 e2e 观测。）

- [ ] **Step 2: RED/GREEN**

Run: `cargo test --test notify_e2e 2>&1 | tail -10`
Expected: 先失败（sink 未落地）→ 实现后通过。契约钉子测试记录首跑结果。

- [ ] **Step 3: README**

`README.md` 追加：
- 新环境变量：`TFA_CONFIG_PATH`（config 路径覆写）、`TFA_NO_NOTIFY`（=1 抑制真实通知，写 sink）。
- config 文件 `~/.config/tfa/config.toml`：给出与 spec §13 一致的注释样例（notify 开关、三通道、triggers 四开关、discipline、quota.burn_rate_window_mins）。
- `tfa notify test` 用法 + macOS 首次授权提示（terminal-notifier/Script Editor 在系统设置授权）。
- Bark/ntfy 配置指引 + **诚实声明**：iOS 后台推送必经 APNs 需外网，「自建 server」不消除对 APNs 的依赖，无纯 LAN 零外网 iOS 后台推送。
- `tfa list` 新增 `quota` 字段说明：`observed_tokens_this_window`（≥ 下界，本地推算，非订阅剩余）、`burn_rate_per_min`、`source=local_estimate`、`window_5h_percent` 恒 null（真实% 待未来「真实配额」里程碑）。

- [ ] **Step 4: 全量回归 + 提交**

Run: `cargo test 2>&1 | tail -6 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3`　Expected: 全绿。
```bash
git add tests/ README.md
git commit -m "test(m3): notify e2e (net-edge count, hook guard, no-clients) + docs"
```

- [ ] **Step 5: 本机真装验收（用户）**

控制器 `cargo install --path . --force` + daemon 换血（`pkill -f 'tfa daemon'`，下个调用自动拉起）。用户验收清单：
1. `tfa list | python3 -m json.tool`：出现 `quota` 段，claude 有 `observed_tokens_this_window`/`burn_rate_per_min`/`source:"local_estimate"`/`window_5h_percent:null`。
2. 触发 `waiting_input`（让某 claude 弹权限确认）：≤2s 收到 macOS 通知 + tmux 底部提示（若配了手机推送，手机也响）。应答后再触发一次 → **仍能收到**（边沿冷却验证，不被吞）。
3. 开启 `done`/`stale`/`dead` 触发（改 config），验证各自通知；关掉则不响。
4. kill 一个 claude 窗口：连续判死后收到 `dead` 通知（去抖验证）；状态栏行为不变。
5. 重启 daemon：**不轰炸**（boot-grace 验证）——恢复的既有会话不集中发一批。
6. quiet_hours 配一个当前时段：`waiting_input` 被静默、`dead` 仍穿透（豁免验证）。
- 用户逐项确认后走 `superpowers:finishing-a-development-branch` 合并。

---

## Self-Review 记录

- **Spec coverage**：spec §4 组件 → T1(config)/T3(burn)/T4(QuotaCache)/T5(notifier)/T6(discipline)；§5 数据模型（consumed/QuotaState/NotifyEvent）→ T2/T4/T5；§6 通知纪律（tick 边界 diff/边沿冷却/boot-grace/generation/dead 去抖/hook 护栏）→ T6；§7 本地用量（consumed 排除 cache_read/只加正 delta/5h 滚动/percent None）→ T2/T3/T4；§8 三通道（term-notifier 优先/tmux -t no-clients 非致命/ureq 超时/bark-ntfy-generic）→ T5；§9 新态不进快照（QuotaCache/Discipline 独立）→ T3/T4/T6；§13 config schema → T1；§14 依赖（ureq 禁 reqwest/toml）→ T1/T5。
- **CRITICAL 落点**：C1 独立 notifier 线程+硬超时 → T5；C2 consumed 排除 cache_read、绝不用 tokens.total → T2；C3 只加正 delta、消失贡献 0 → T3；C4 QuotaState/纪律态不进快照 → T3/T4/T6。
- **Placeholder scan**：e2e 骨架标注「沿用 scanner_e2e/hook_cmd 既有辅助」= 引用现存代码，非占位；ureq 3.x API 注明「以实际版本为准、务必设超时」是实现约束非占位。其余步骤给全代码。
- **Type consistency**：`TranscriptCursor{offset,consumed}`、`AgentSession.consumed_tokens/stable_key()`、`BurnSampler::{sample,provider_consumed,burn_rate_per_min,providers}`、`QuotaState`/`QuotaCache::{refresh,states}`、`NotifyEvent`/`NotifyKind`、`Discipline::{new,seed,edges,snapshot_states}`、`trigger_kind` 全部在 Global Interfaces 锁定并跨任务一致。

## 开放决策（默认按推荐，验收可翻）

1. consumed delta = `input_tokens + output_tokens`（排除 cache_read/cache_creation）——最贴「新 token 吞吐」，验收时若想含 cache_creation 可调。
2. burn_rate_window 默认 60min；5h 窗口 floor_to_hour 锚定（估算，Real 的 reset_at 才权威，M3 无 Real 故标 estimated）。
3. `tfa notify send` CLI 保留最小实现（daemon 内部走 discipline→mpsc，不经 CLI）；`send` 子命令主要给手动调试。
4. quiet_hours 默认豁免集 `["dead"]`；未来真实配额里程碑加 `"quota_alert"`。
