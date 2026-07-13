# tfa Real Quota Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 spec `docs/superpowers/specs/2026-07-13-tfa-real-quota-design.md` 实现真实订阅配额：opt-in 的 daemon 内 fetcher 调 OAuth usage 接口，真实百分比进快照/TUI/状态栏，quota_alert 迟滞告警。

**Architecture:** daemon 内独立 fetcher 线程（与 notifier 同构，仅 `config.quota.real=true` 时 spawn）；token 只在该线程内存（newtype 脱敏）；结果写 `Arc<Mutex<RealQuotaCell>>`，快照组装时与 LocalEstimate 合并（30min TTL 粘滞降级）；告警在 fetcher 侧做迟滞后走既有 notify mpsc。

**Tech Stack:** Rust、ureq（既有）、serde_json（既有）、std::thread+mpsc、libc（本地时区）。**零新增依赖。**

## Global Constraints

（每个任务隐含包含本节；逐条来自 spec）

- **默认关 = 行为零变化**：`config.quota.real` 默认 `false`；不开启则 fetcher 线程不 spawn、零 Keychain 读、零网络调用；现有 184 测试零改动通过（有测试钉）。
- **请求纪律（spec §6）**：单飞；超时 5s；`refresh_secs` 默认 600 下限钳 300；任何失败一次即退避 10m→20m→40m→80m→2h 封顶，成功复位；429 取 max(退避, Retry-After)；401/403 重读一次 Keychain 后按退避走，绝不循环重试；解析失败=失败。
- **token 卫生**：`AccessToken` newtype，手写 Debug/Display 输出 `AccessToken(***)`，不实现 Serialize；绝不进快照/日志/`tfa list`（有 grep 级测试钉）。
- **诚实性（spec §7）**：真实值粘滞 TTL=1800s，过期回落 LocalEstimate；UI 永远内联 source 标签；估算与真实永不混淆。
- 快照只做加法（`#[serde(default)]`，老快照兼容测试照钉）；`QuotaSource` 加变体的向前兼容注记见 spec §5（可接受）。
- no-async 铁律（no_async_gate 照钉）；绝不修改 `src/client.rs`；hook 路径零改动。
- UI 新串全部进 i18n `Texts`（EN/ZH 双份）；KEYBINDINGS 四拷贝一致性测试不受影响（本里程碑不碰键位）。
- 零警告；`cargo clippy --all-targets -- -D warnings` 零发现；conventional commits（`feat(quota):` / `test(quota):` 等）。
- 本 crate 纯 binary：纯逻辑断言写模块内 `#[cfg(test)]`；集成测试放 `tests/`。
- ureq 3.x builder API 以 `src/notify/channels.rs` 现用调用形态为准适配（行为断言不变）。

## File Structure

| 文件 | 职责 |
|---|---|
| `src/quota/real.rs`（新） | AccessToken、凭证链、ISO8601/响应解析、Backoff、FetchOutcome+fetch、RealQuotaCell、merge、AlertArm、fetcher 线程 |
| `src/quota/mod.rs` | `pub mod real;`、QuotaState 加字段、QuotaSource::RealApi |
| `src/config.rs` | QuotaConfig 扩展（real/refresh_secs/status_bar_percent/alert_5h/alert_7d）|
| `src/daemon/mod.rs` | 条件 spawn fetcher，持 RealQuotaCell |
| `src/daemon/server.rs` | Snapshot 组装处调用 merge |
| `src/notify/mod.rs` | NotifyKind::QuotaAlert |
| `src/notify/channels.rs` | tmux_send 空 pane_id 分支 |
| `src/tui/view.rs` + `src/tui/i18n.rs` | header/详情真实 %、本地时区渲染、新 Texts 字段 |
| `src/render.rs` + `src/commands/status.rs` | status_bar_percent chip |
| `tests/quota_real_e2e.rs`（新） | 默认关零行为 + notify-sink 断言 |
| `fixtures/oauth_usage.json`（新） | 真机响应 fixture（任务 1 授权门产物） |

---

### Task 1: ISO8601 解析器 + 响应解析器 + 真机 fixture（用户授权门）

**Files:**
- Create: `src/quota/real.rs`（本任务只含解析部分）
- Modify: `src/quota/mod.rs`（加 `pub mod real;`）
- Create: `fixtures/oauth_usage.json`

**Interfaces:**
- Consumes: 无。
- Produces: `real::parse_iso8601_ms(s: &str) -> Option<u64>`；`real::RealUsage { five_hour_pct: f64, five_hour_resets_ms: u64, seven_day_pct: f64, seven_day_resets_ms: u64, seven_day_sonnet_pct: Option<f64>, fetched_at_ms: u64 }`（Clone+Debug）；`real::parse_usage_response(body: &str, now_ms: u64) -> Option<RealUsage>`。

**⚠ 授权门（spec §12）**：本任务派单时 controller 会向用户申请一次性授权（读 Keychain token + 单次 curl），把真实响应体（经脱敏检查）交给你写入 `fixtures/oauth_usage.json`。若派单信息里没有附 fixture 内容，用 spec §3.2 的文档形状构造，并在报告 Concerns 里注明「fixture 非真机产物」。**你自己绝不读 Keychain、绝不发网络请求。**

- [ ] **Step 1: 写失败的解析测试**

`src/quota/mod.rs` 末尾（`pub mod burn;` 旁）加 `pub mod real;`。新建 `src/quota/real.rs`：

```rust
//! 真实配额（spec: docs/superpowers/specs/2026-07-13-tfa-real-quota-design.md）。
//! 本文件承载与非官方 oauth/usage 接口相关的全部逻辑；`config.quota.real=false`
//! （默认）时本模块除类型定义外的任何代码都不会被 daemon 触达。

#[derive(Debug, Clone, PartialEq)]
pub struct RealUsage {
    pub five_hour_pct: f64,
    pub five_hour_resets_ms: u64,
    pub seven_day_pct: f64,
    pub seven_day_resets_ms: u64,
    pub seven_day_sonnet_pct: Option<f64>,
    pub fetched_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_parses_utc_and_offset_forms() {
        // 2026-03-27T10:00:00+00:00 = 1774605600000（手算：见 days_from_civil 注释）
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00+00:00"), Some(1_774_605_600_000));
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00Z"), Some(1_774_605_600_000));
        // 正偏移：本地 10:00 @ +08:00 = UTC 02:00 → 减 8h
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00+08:00"), Some(1_774_605_600_000 - 8 * 3_600_000));
        // 毫秒小数容忍
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00.500Z"), Some(1_774_605_600_500));
        // 闰年 2 月
        assert_eq!(parse_iso8601_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        // 垃圾输入
        assert_eq!(parse_iso8601_ms("not a date"), None);
        assert_eq!(parse_iso8601_ms("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn usage_response_parses_fixture_shape() {
        let body = include_str!("../../fixtures/oauth_usage.json");
        let u = parse_usage_response(body, 42).expect("fixture parses");
        assert!(u.five_hour_pct >= 0.0 && u.five_hour_pct <= 100.0);
        assert!(u.five_hour_resets_ms > 1_500_000_000_000, "resets_at 是合理的未来 epoch ms");
        assert!(u.seven_day_resets_ms >= u.five_hour_resets_ms, "7d 重置不早于 5h");
        assert_eq!(u.fetched_at_ms, 42);
    }

    #[test]
    fn usage_response_tolerates_missing_sonnet_and_unknown_fields() {
        let body = r#"{"five_hour":{"utilization":18.0,"resets_at":"2026-03-27T10:00:00+00:00"},
                       "seven_day":{"utilization":17.0,"resets_at":"2026-04-02T13:00:00+00:00"},
                       "future_window":{"whatever":1}}"#;
        let u = parse_usage_response(body, 0).expect("parses without sonnet");
        assert_eq!(u.seven_day_sonnet_pct, None);
        assert!((u.five_hour_pct - 18.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_response_rejects_malformed() {
        assert!(parse_usage_response("not json", 0).is_none());
        assert!(parse_usage_response(r#"{"five_hour":{}}"#, 0).is_none());
    }
}
```

`fixtures/oauth_usage.json` 写入派单时提供的真机响应体（或 §3.2 文档形状）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test quota::real`
Expected: FAIL（编译错误：`parse_iso8601_ms`/`parse_usage_response` 不存在）

- [ ] **Step 3: 实现两个解析器**

在 `real.rs` 的 `RealUsage` 下方：

```rust
/// Howard Hinnant days-from-civil：公历日期 → 距 1970-01-01 的天数。纯整数算术。
/// 例：2026-03-27 → 20539 天；20539*86400 + 10*3600 = 1774605600（秒）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 解析接口用的 ISO8601 形态：`YYYY-MM-DDTHH:MM:SS[.fff...][Z|±HH:MM]` → epoch ms。
/// 只服务本接口，不追求通用；任何不符即 None（调用方按解析失败退避，spec §6.5）。
pub fn parse_iso8601_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 { return None; }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' { return None; }
    let (y, mo, d) = (num(0..4)?, num(5..7)? as u32, num(8..10)? as u32);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 { return None; }
    // 小数秒（可选）→ 毫秒
    let mut i = 19;
    let mut frac_ms: i64 = 0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() { end += 1; }
        if end == start { return None; }
        let digits = &s[start..end.min(start + 3)];
        frac_ms = digits.parse::<i64>().ok()? * 10_i64.pow(3 - digits.len() as u32);
        i = end;
    }
    // 时区
    let offset_secs: i64 = match b.get(i) {
        Some(b'Z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            if b.len() != i + 6 || b[i + 3] != b':' { return None; }
            let oh: i64 = s.get(i + 1..i + 3)?.parse().ok()?;
            let om: i64 = s.get(i + 4..i + 6)?.parse().ok()?;
            let v = oh * 3600 + om * 60;
            if *sign == b'+' { v } else { -v }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h * 3600 + mi * 60 + sec - offset_secs;
    if secs < 0 { return None; }
    Some(secs as u64 * 1000 + frac_ms as u64)
}

/// 响应体 → RealUsage。容忍未知字段/缺席的 sonnet 窗（spec §3.2）；
/// 必需窗缺失或形状不对 → None（调用方视为失败并退避）。
pub fn parse_usage_response(body: &str, now_ms: u64) -> Option<RealUsage> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let win = |key: &str| -> Option<(f64, u64)> {
        let w = v.get(key)?;
        let pct = w.get("utilization")?.as_f64()?.clamp(0.0, 100.0);
        let resets = parse_iso8601_ms(w.get("resets_at")?.as_str()?)?;
        Some((pct, resets))
    };
    let (five_pct, five_resets) = win("five_hour")?;
    let (seven_pct, seven_resets) = win("seven_day")?;
    Some(RealUsage {
        five_hour_pct: five_pct,
        five_hour_resets_ms: five_resets,
        seven_day_pct: seven_pct,
        seven_day_resets_ms: seven_resets,
        seven_day_sonnet_pct: win("seven_day_sonnet").map(|w| w.0),
        fetched_at_ms: now_ms,
    })
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test quota::real`
Expected: PASS（6 断言组全绿）。再跑 `cargo test` 全量 + `cargo clippy --all-targets -- -D warnings`。

- [ ] **Step 5: Commit**

```bash
git add src/quota/real.rs src/quota/mod.rs fixtures/oauth_usage.json
git commit -m "feat(quota): ISO8601/usage-response parsers + real-response fixture"
```

---

### Task 2: config 扩展 + AccessToken + 凭证链

**Files:**
- Modify: `src/config.rs`（QuotaConfig 扩展）
- Modify: `src/quota/real.rs`（AccessToken + 凭证链）

**Interfaces:**
- Consumes: Task 1 的 `real.rs`。
- Produces: `QuotaConfig { burn_rate_window_mins: u64, real: bool, refresh_secs: u64, status_bar_percent: bool, alert_5h: u8, alert_7d: u8 }`（Default: 60/false/600/false/85/90）；`real::AccessToken`（`pub fn secret(&self) -> &str`，Debug/Display=`AccessToken(***)`）；`real::parse_credentials_json(s: &str) -> Option<AccessToken>`；`real::read_credentials() -> Option<AccessToken>`（链：`security` CLI → `~/.claude/.credentials.json` → env `CLAUDE_CODE_OAUTH_TOKEN`）；`real::effective_refresh_secs(cfg: &QuotaConfig) -> u64`（下限钳 300）。

- [ ] **Step 1: 写失败测试**

`src/config.rs` tests 模块追加：

```rust
    #[test]
    fn quota_real_defaults_off_and_extended_fields_parse() {
        let c = Config::from_toml_str("");
        assert!(!c.quota.real, "real 默认必须是 false（零 API 承诺）");
        assert_eq!(c.quota.refresh_secs, 600);
        assert!(!c.quota.status_bar_percent);
        assert_eq!((c.quota.alert_5h, c.quota.alert_7d), (85, 90));
        let c = Config::from_toml_str("[quota]\nreal = true\nrefresh_secs = 120\nalert_5h = 70\n");
        assert!(c.quota.real);
        assert_eq!(c.quota.refresh_secs, 120, "原样存储，钳制在使用侧");
        assert_eq!(c.quota.alert_5h, 70);
        assert_eq!(c.quota.burn_rate_window_mins, 60, "旧字段不受影响");
    }
```

`src/quota/real.rs` tests 追加：

```rust
    #[test]
    fn access_token_never_leaks_via_debug_or_display() {
        let t = AccessToken::new("sk-ant-oat01-SECRET".into());
        assert_eq!(format!("{t:?}"), "AccessToken(***)");
        assert_eq!(format!("{t}"), "AccessToken(***)");
        assert_eq!(t.secret(), "sk-ant-oat01-SECRET");
    }

    #[test]
    fn credentials_json_extracts_access_token() {
        let j = r#"{"claudeAiOauth":{"accessToken":"tok123","refreshToken":"r","scopes":[]}}"#;
        assert_eq!(parse_credentials_json(j).unwrap().secret(), "tok123");
        assert!(parse_credentials_json("{}").is_none());
        assert!(parse_credentials_json("junk").is_none());
    }

    #[test]
    fn refresh_secs_clamped_to_300_floor() {
        let mut q = crate::config::QuotaConfig::default();
        q.refresh_secs = 60;
        assert_eq!(effective_refresh_secs(&q), 300);
        q.refresh_secs = 900;
        assert_eq!(effective_refresh_secs(&q), 900);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test quota_real_defaults; cargo test quota::real`
Expected: FAIL（字段/类型不存在）

- [ ] **Step 3: 实现**

`src/config.rs`：QuotaConfig 换成（含手写 Default，模式同 TuiConfig 的注释理由——非零默认值）：

```rust
/// `[quota]`：`burn_rate_window_mins` 为 M3 本地估算参数。其余为真实配额
/// （2026-07-13 spec）：`real` 默认 false = 不 spawn fetcher、零 Keychain/网络
/// （用户风险决策存档见 spec §2）；`refresh_secs` 使用侧钳 ≥300
/// （`quota::real::effective_refresh_secs`）；alert_* 为 quota_alert 阈值，0=关。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub burn_rate_window_mins: u64,
    pub real: bool,
    pub refresh_secs: u64,
    pub status_bar_percent: bool,
    pub alert_5h: u8,
    pub alert_7d: u8,
}
impl Default for QuotaConfig {
    fn default() -> Self {
        Self { burn_rate_window_mins: 60, real: false, refresh_secs: 600,
               status_bar_percent: false, alert_5h: 85, alert_7d: 90 }
    }
}
```

（删除原单行 `impl Default for QuotaConfig`。）`src/quota/real.rs` 追加：

```rust
/// OAuth token 密封盒：Debug/Display 打码、无 Serialize——token 只允许经
/// `secret()` 流向 HTTP Authorization 头（spec 全局约束「token 卫生」）。
pub struct AccessToken(String);
impl AccessToken {
    pub fn new(s: String) -> Self { Self(s) }
    pub fn secret(&self) -> &str { &self.0 }
}
impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("AccessToken(***)") }
}
impl std::fmt::Display for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("AccessToken(***)") }
}

pub fn parse_credentials_json(s: &str) -> Option<AccessToken> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let tok = v.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
    (!tok.is_empty()).then(|| AccessToken::new(tok.to_string()))
}

/// 凭证链（spec §3.3）：Keychain → ~/.claude/.credentials.json → env。
/// 任一环节失败静默走下一环；全失败 None（调用方按失败退避，绝不 panic）。
pub fn read_credentials() -> Option<AccessToken> {
    let keychain = std::process::Command::new("security")
        .args(["find-generic-password", "-a"])
        .arg(std::env::var("USER").unwrap_or_default())
        .args(["-s", "Claude Code-credentials", "-w"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_credentials_json(s.trim()));
    if keychain.is_some() { return keychain; }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    if let Ok(s) = std::fs::read_to_string(home.join(".claude/.credentials.json")) {
        if let Some(t) = parse_credentials_json(&s) { return Some(t); }
    }
    std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok().filter(|s| !s.is_empty()).map(AccessToken::new)
}

pub fn effective_refresh_secs(cfg: &crate::config::QuotaConfig) -> u64 {
    cfg.refresh_secs.max(300)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test` + clippy。Expected: 全绿零警告。（`read_credentials` 不写单测——环境依赖；由 Task 3 的 401 路径与真机验收覆盖。）

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/quota/real.rs
git commit -m "feat(quota): [quota] config extension + AccessToken sealing + credential chain"
```

---

### Task 3: fetcher 线程（请求纪律全套 + mock HTTP 测试）

**Files:**
- Modify: `src/quota/real.rs`（Backoff、FetchOutcome、fetch、RealQuotaCell、spawn）
- Modify: `src/daemon/mod.rs`（条件 spawn + 持 cell）

**Interfaces:**
- Consumes: Task 1/2 全部。
- Produces: `real::Backoff`（`new()`、`on_success(&mut)`、`on_failure(&mut)`、`delay_secs(&self, base: u64) -> u64`——失败 0 次返 base，n 次返 min(600·2^(n-1), 7200)）；`real::FetchOutcome { Ok(RealUsage), AuthFailed, RateLimited { retry_after_secs: Option<u64> }, Failed }`；`real::fetch(url: &str, token: &AccessToken, now_ms: u64) -> FetchOutcome`；`real::RealQuotaCell { pub usage: Option<(RealUsage, std::time::Instant)> }`（Default）；`real::spawn(cell: Arc<Mutex<RealQuotaCell>>, cfg: Arc<Mutex<Config>>, tx: Sender<NotifyEvent>)`。daemon 持 `real_cell: Arc<Mutex<RealQuotaCell>>` 并传给 server（Task 4 接线用）。

- [ ] **Step 1: 写失败的 Backoff/fetch 测试**

`real.rs` tests 追加（mock server 用 std::net::TcpListener 手写，零新依赖）：

```rust
    #[test]
    fn backoff_ladder_10m_to_2h_cap_and_reset() {
        let mut b = Backoff::new();
        assert_eq!(b.delay_secs(600), 600, "无失败=正常间隔");
        b.on_failure();
        assert_eq!(b.delay_secs(600), 600, "第 1 次失败=10m");
        b.on_failure();
        assert_eq!(b.delay_secs(600), 1200);
        b.on_failure();
        assert_eq!(b.delay_secs(600), 2400);
        b.on_failure(); b.on_failure(); b.on_failure();
        assert_eq!(b.delay_secs(600), 7200, "封顶 2h");
        b.on_success();
        assert_eq!(b.delay_secs(600), 600, "成功复位");
        // 基础间隔大于退避值时取大者（用户配了 3600 就不该被退避降到更小）
        let mut b = Backoff::new();
        b.on_failure();
        assert_eq!(b.delay_secs(3600), 3600);
    }

    /// 一次性 mock HTTP server：接受 1 个连接，回写给定响应，返回 (url, join)。
    fn mock_http(status_line: &'static str, extra_headers: &'static str, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/oauth/usage", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf); // 读掉请求头，不解析
                let resp = format!("{status_line}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (url, h)
    }

    #[test]
    fn fetch_ok_parses_real_usage() {
        let body = r#"{"five_hour":{"utilization":62.0,"resets_at":"2026-03-27T10:00:00Z"},"seven_day":{"utilization":31.0,"resets_at":"2026-04-02T13:00:00Z"}}"#;
        let (url, h) = mock_http("HTTP/1.1 200 OK", "content-type: application/json\r\n", body);
        let out = fetch(&url, &AccessToken::new("t".into()), 7);
        h.join().unwrap();
        match out {
            FetchOutcome::Ok(u) => { assert!((u.five_hour_pct - 62.0).abs() < f64::EPSILON); assert_eq!(u.fetched_at_ms, 7); }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn fetch_maps_auth_rate_limit_and_garbage() {
        let (url, h) = mock_http("HTTP/1.1 401 Unauthorized", "", "{}");
        assert!(matches!(fetch(&url, &AccessToken::new("t".into()), 0), FetchOutcome::AuthFailed));
        h.join().unwrap();
        let (url, h) = mock_http("HTTP/1.1 429 Too Many Requests", "retry-after: 120\r\n", "{}");
        match fetch(&url, &AccessToken::new("t".into()), 0) {
            FetchOutcome::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, Some(120)),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        h.join().unwrap();
        let (url, h) = mock_http("HTTP/1.1 200 OK", "", "not json");
        assert!(matches!(fetch(&url, &AccessToken::new("t".into()), 0), FetchOutcome::Failed));
        h.join().unwrap();
        // 连不上（端口已关）
        assert!(matches!(fetch("http://127.0.0.1:1/x", &AccessToken::new("t".into()), 0), FetchOutcome::Failed));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test quota::real`
Expected: FAIL（类型不存在）

- [ ] **Step 3: 实现 Backoff/FetchOutcome/fetch/cell/spawn**

`real.rs` 追加：

```rust
/// 失败退避阶梯（spec §6.4）：10m→20m→40m→80m→2h 封顶；成功复位。
/// 返回值与基础间隔取 max——退避绝不把节奏加快。
pub struct Backoff { failures: u32 }
impl Backoff {
    pub fn new() -> Self { Self { failures: 0 } }
    pub fn on_success(&mut self) { self.failures = 0; }
    pub fn on_failure(&mut self) { self.failures = self.failures.saturating_add(1); }
    pub fn delay_secs(&self, base: u64) -> u64 {
        if self.failures == 0 { return base; }
        let backoff = 600u64.saturating_mul(1 << (self.failures - 1).min(4)).min(7200);
        backoff.max(base)
    }
}
impl Default for Backoff { fn default() -> Self { Self::new() } }

#[derive(Debug)]
pub enum FetchOutcome {
    Ok(RealUsage),
    AuthFailed,
    RateLimited { retry_after_secs: Option<u64> },
    Failed,
}

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const FETCH_TIMEOUT_SECS: u64 = 5;

/// 单次请求（spec §6.3：超时 5s；§3.1 头集合）。url 参数化以便 mock 测试。
/// ureq 3.x：非 2xx 默认作 Err 返回——用 http_status_as_error(false) 统一拿状态码
/// （以 channels.rs 现用 API 形态为准，编译不过时只调 builder 写法、行为不变）。
pub fn fetch(url: &str, token: &AccessToken, now_ms: u64) -> FetchOutcome {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS)))
        .build();
    let agent: ureq::Agent = config.into();
    let resp = agent.get(url)
        .header("Authorization", &format!("Bearer {}", token.secret()))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .header("User-Agent", concat!("tfa/", env!("CARGO_PKG_VERSION")))
        .call();
    let mut resp = match resp { Ok(r) => r, Err(_) => return FetchOutcome::Failed };
    match resp.status().as_u16() {
        200..=299 => {}
        401 | 403 => return FetchOutcome::AuthFailed,
        429 => {
            let retry_after_secs = resp.headers().get("retry-after")
                .and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse().ok());
            return FetchOutcome::RateLimited { retry_after_secs };
        }
        _ => return FetchOutcome::Failed,
    }
    let body = match resp.body_mut().read_to_string() { Ok(b) => b, Err(_) => return FetchOutcome::Failed };
    match parse_usage_response(&body, now_ms) { Some(u) => FetchOutcome::Ok(u), None => FetchOutcome::Failed }
}

/// fetcher 与快照组装间的共享格子。Instant 是单调钟：TTL 判断不受 wall clock 影响。
#[derive(Default)]
pub struct RealQuotaCell {
    pub usage: Option<(RealUsage, std::time::Instant)>,
}

/// fetcher 线程（spec §4）：仅 config.quota.real=true 时由 daemon spawn。
/// 严格单飞：线程内串行 loop。alert 评估在 Task 6 接入（本任务 tx 仅占位持有）。
pub fn spawn(
    cell: std::sync::Arc<std::sync::Mutex<RealQuotaCell>>,
    cfg: std::sync::Arc<std::sync::Mutex<crate::config::Config>>,
    tx: std::sync::mpsc::Sender<crate::notify::NotifyEvent>,
) {
    std::thread::spawn(move || {
        let _tx = tx; // Task 6 (quota_alert) 接入
        let mut token = read_credentials();
        let mut backoff = Backoff::new();
        loop {
            let q = { cfg.lock().unwrap_or_else(std::sync::PoisonError::into_inner).quota.clone() };
            let base = effective_refresh_secs(&q);
            let mut sleep_secs = backoff.delay_secs(base);
            match &token {
                None => {
                    // 无凭证：按失败退避重试读取（用户可能稍后授权 Keychain）
                    backoff.on_failure();
                    sleep_secs = backoff.delay_secs(base);
                    token = read_credentials();
                }
                Some(t) => match fetch(USAGE_URL, t, crate::daemon::now_ms()) {
                    FetchOutcome::Ok(u) => {
                        backoff.on_success();
                        sleep_secs = backoff.delay_secs(base);
                        cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            .usage = Some((u, std::time::Instant::now()));
                    }
                    FetchOutcome::AuthFailed => {
                        token = read_credentials(); // token 轮换自愈：重读一次
                        backoff.on_failure();
                        sleep_secs = backoff.delay_secs(base);
                    }
                    FetchOutcome::RateLimited { retry_after_secs } => {
                        backoff.on_failure();
                        sleep_secs = backoff.delay_secs(base).max(retry_after_secs.unwrap_or(0));
                    }
                    FetchOutcome::Failed => {
                        backoff.on_failure();
                        sleep_secs = backoff.delay_secs(base);
                    }
                },
            }
            std::thread::sleep(std::time::Duration::from_secs(sleep_secs));
        }
    });
}
```

`src/daemon/mod.rs`：`now_ms` 若非 pub 改为 `pub(crate)`；config Arc 建立后（约 34 行）追加：

```rust
    let real_quota = std::sync::Arc::new(std::sync::Mutex::new(crate::quota::real::RealQuotaCell::default()));
    if config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).quota.real {
        crate::quota::real::spawn(std::sync::Arc::clone(&real_quota), std::sync::Arc::clone(&config), notify_tx.clone());
    }
```

（`real_quota` 先建后传 server——传参接线在 Task 4；本任务保证编译：变量加 `let _ = &real_quota;` 或直接本任务就把参数穿进 server 签名但 server 暂不使用——选后者，减少下任务churn：`server::serve(..., Arc::clone(&real_quota), ...)` 加参、server 端 `_real_quota` 占位。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test` + clippy。Expected: 全绿零警告（mock 测试 5 个新断言组）。

- [ ] **Step 5: Commit**

```bash
git add src/quota/real.rs src/daemon/mod.rs src/daemon/server.rs
git commit -m "feat(quota): real-usage fetcher thread — single-flight, backoff ladder, 429/401 discipline"
```

---

### Task 4: 快照合并（QuotaState 加字段 + RealApi + TTL 粘滞）

**Files:**
- Modify: `src/quota/mod.rs`（QuotaState 字段 + QuotaSource::RealApi）
- Modify: `src/quota/real.rs`（merge + TTL）
- Modify: `src/daemon/server.rs`（Snapshot 组装处接线）

**Interfaces:**
- Consumes: Task 3 的 `RealQuotaCell`。
- Produces: `QuotaState` 新字段 `weekly_sonnet_percent: Option<u8>`、`weekly_reset_at_ms: Option<u64>`（均 `#[serde(default)]`）；`QuotaSource::RealApi`（serde `"real_api"`）；`real::TTL_SECS: u64 = 1800`；`real::merge(local: Vec<QuotaState>, cell: &Mutex<RealQuotaCell>, now_ms: u64) -> Vec<QuotaState>`——Claude 条目且 cell 年龄<TTL 时覆盖：`window_5h_percent=Some(round(five_hour_pct))`、`weekly_percent`、`weekly_sonnet_percent`、`reset_at_ms=Some(five_hour_resets_ms)`、`weekly_reset_at_ms`、`reset_estimated=false`、`source=RealApi`、`freshness_ms=usage.fetched_at_ms`；`observed_tokens_this_window`/`burn_rate_per_min` 保留本地值（spec §5 合并语义）。

- [ ] **Step 1: 写失败测试**

`src/quota/mod.rs` tests 追加：

```rust
    #[test]
    fn quota_state_new_fields_default_and_old_json_loads() {
        // 老快照（无新字段）必须能反序列化——快照只做加法
        let old = r#"{"provider":"claude","window_5h_percent":null,"weekly_percent":null,
            "reset_at_ms":1,"reset_estimated":true,"observed_tokens_this_window":5,
            "burn_rate_per_min":1.0,"source":"local_estimate","freshness_ms":2}"#;
        let q: QuotaState = serde_json::from_str(old).unwrap();
        assert_eq!(q.weekly_sonnet_percent, None);
        assert_eq!(q.weekly_reset_at_ms, None);
        // RealApi 变体 wire 形状
        assert_eq!(serde_json::to_string(&QuotaSource::RealApi).unwrap(), r#""real_api""#);
    }
```

`src/quota/real.rs` tests 追加：

```rust
    fn local_state() -> crate::quota::QuotaState {
        crate::quota::QuotaState {
            provider: crate::event::AgentKind::Claude,
            window_5h_percent: None, weekly_percent: None,
            reset_at_ms: Some(1), reset_estimated: true,
            observed_tokens_this_window: 12345, burn_rate_per_min: 9.5,
            source: crate::quota::QuotaSource::LocalEstimate, freshness_ms: 100,
            weekly_sonnet_percent: None, weekly_reset_at_ms: None,
        }
    }
    fn usage() -> RealUsage {
        RealUsage { five_hour_pct: 62.4, five_hour_resets_ms: 111, seven_day_pct: 31.0,
                    seven_day_resets_ms: 222, seven_day_sonnet_pct: Some(10.0), fetched_at_ms: 999 }
    }

    #[test]
    fn merge_overlays_fresh_real_and_keeps_local_observed() {
        let cell = std::sync::Mutex::new(RealQuotaCell { usage: Some((usage(), std::time::Instant::now())) });
        let out = merge(vec![local_state()], &cell, 0);
        let q = &out[0];
        assert_eq!(q.window_5h_percent, Some(62));
        assert_eq!(q.weekly_percent, Some(31));
        assert_eq!(q.weekly_sonnet_percent, Some(10));
        assert_eq!((q.reset_at_ms, q.weekly_reset_at_ms), (Some(111), Some(222)));
        assert!(!q.reset_estimated);
        assert!(matches!(q.source, crate::quota::QuotaSource::RealApi));
        assert_eq!(q.freshness_ms, 999);
        assert_eq!(q.observed_tokens_this_window, 12345, "本地观测保留");
        assert!((q.burn_rate_per_min - 9.5).abs() < f64::EPSILON, "burn 保留");
    }

    #[test]
    fn merge_expired_ttl_leaves_local_untouched() {
        let stale = std::time::Instant::now() - std::time::Duration::from_secs(TTL_SECS + 1);
        let cell = std::sync::Mutex::new(RealQuotaCell { usage: Some((usage(), stale)) });
        let out = merge(vec![local_state()], &cell, 0);
        assert!(matches!(out[0].source, crate::quota::QuotaSource::LocalEstimate));
        assert_eq!(out[0].window_5h_percent, None, "过期绝不渲染假百分比");
        let empty = std::sync::Mutex::new(RealQuotaCell::default());
        let out = merge(vec![local_state()], &empty, 0);
        assert!(matches!(out[0].source, crate::quota::QuotaSource::LocalEstimate));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test quota`
Expected: FAIL（字段/变体不存在）

- [ ] **Step 3: 实现**

`src/quota/mod.rs`：`QuotaSource` 加变体 `RealApi`（枚举已有 `#[serde(rename_all = "snake_case")]`）；`QuotaState` 末尾加：

```rust
    #[serde(default)]
    pub weekly_sonnet_percent: Option<u8>,
    #[serde(default)]
    pub weekly_reset_at_ms: Option<u64>,
```

（`QuotaCache::refresh` 构造处补 `weekly_sonnet_percent: None, weekly_reset_at_ms: None`。）`real.rs` 追加：

```rust
/// 真实值粘滞 TTL（spec §7）：Instant 单调钟计龄，过期即回落本地估算。
pub const TTL_SECS: u64 = 1800;

pub fn merge(
    mut local: Vec<crate::quota::QuotaState>,
    cell: &std::sync::Mutex<RealQuotaCell>,
    _now_ms: u64,
) -> Vec<crate::quota::QuotaState> {
    let guard = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((u, at)) = guard.usage.as_ref() else { return local };
    if at.elapsed().as_secs() >= TTL_SECS { return local; }
    for q in local.iter_mut().filter(|q| q.provider == crate::event::AgentKind::Claude) {
        q.window_5h_percent = Some(u.five_hour_pct.round().clamp(0.0, 100.0) as u8);
        q.weekly_percent = Some(u.seven_day_pct.round().clamp(0.0, 100.0) as u8);
        q.weekly_sonnet_percent = u.seven_day_sonnet_pct.map(|p| p.round().clamp(0.0, 100.0) as u8);
        q.reset_at_ms = Some(u.five_hour_resets_ms);
        q.weekly_reset_at_ms = Some(u.seven_day_resets_ms);
        q.reset_estimated = false;
        q.source = crate::quota::QuotaSource::RealApi;
        q.freshness_ms = u.fetched_at_ms;
    }
    local
}
```

`src/daemon/server.rs` `Request::Snapshot` 分支改为：

```rust
        Request::Snapshot => {
            let sessions = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions();
            let quota = quota.lock().unwrap_or_else(std::sync::PoisonError::into_inner).states();
            let quota = crate::quota::real::merge(quota, real_quota, super::now_ms());
            Response::Snapshot { sessions, quota, generated_at_ms: super::now_ms() }
        }
```

（Task 3 已把 `real_quota: &Mutex<RealQuotaCell>` 穿进 server 各函数签名，本任务去掉占位下划线。）

**边界情况**：本地估算只有在 Claude 会话产生过消耗后才会有 Claude 条目——real=true 但还没有任何本地条目时，merge 后为空、TUI 无 quota 段。可接受（有 agent 活动后立即出现）；在 spec §14 已知限制处由 Task 6 补一句注记。

- [ ] **Step 4: 跑测试确认通过 + 兼容全量**

Run: `cargo test` + clippy。Expected: 全绿（含既有 m1 快照兼容测试）。

- [ ] **Step 5: Commit**

```bash
git add src/quota/mod.rs src/quota/real.rs src/daemon/server.rs
git commit -m "feat(quota): snapshot merge — RealApi overlay with 30min sticky TTL, additive QuotaState fields"
```

---

### Task 5: 展示（TUI header/详情 + i18n + status bar chip）

**Files:**
- Modify: `src/tui/view.rs`、`src/tui/i18n.rs`
- Modify: `src/render.rs`、`src/commands/status.rs`

**Interfaces:**
- Consumes: Task 4 的 QuotaState 新字段/RealApi。
- Produces: view 纯函数 `fmt_local_hm(ms: u64) -> String`（libc localtime_r → `HH:MM`）；header 真实态 `claude 5h 62%·7d 31%`（源=RealApi 时替换 burn 段，burn 移详情）；详情三窗口+重置时刻+数据年龄+source；`render::render_tmux(sessions, now_ms, quota_pct: Option<u8>) -> String`（**签名变更**，Some 时追加 ` {p}%` chip）；`commands::status::run` 读 config，`status_bar_percent=true` 且快照含 RealApi Claude 条目时传 Some。

- [ ] **Step 1: 写失败测试**

`src/tui/view.rs` tests 追加（沿用现有 quota/render_text 夹具风格；构造 QuotaState 补上两个新字段）：

```rust
    #[test]
    fn header_shows_real_percents_when_source_is_real_api() {
        let mut q = quota(340_000, 552.0);
        q.window_5h_percent = Some(62); q.weekly_percent = Some(31);
        q.source = crate::quota::QuotaSource::RealApi;
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![q], 0);
        let text = render_text(&m, 120, 30);
        assert!(text.contains("claude 5h 62%·7d 31%"), "real header:\n{text}");
        assert!(!text.contains("tok/min") || !text.contains("claude 552"), "burn 不再占 header：\n{text}");
    }

    #[test]
    fn header_keeps_burn_for_local_estimate() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![quota(340_000, 552.0)], 0);
        let text = render_text(&m, 120, 30);
        assert!(text.contains("552 tok/min"), "本地估算保持原样:\n{text}");
    }

    #[test]
    fn detail_shows_three_windows_age_and_source() {
        let mut q = quota(340_000, 9.5);
        q.window_5h_percent = Some(62); q.weekly_percent = Some(31);
        q.weekly_sonnet_percent = Some(10);
        q.reset_at_ms = Some(1_774_605_600_000); q.weekly_reset_at_ms = Some(1_774_700_000_000);
        q.source = crate::quota::QuotaSource::RealApi;
        let mut m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![q], 60_000);
        m.generated_at_ms = 60_000;
        let text = render_text(&m, 130, 34);
        assert!(text.contains("5h 62%"), "detail 5h:\n{text}");
        assert!(text.contains("7d 31%"), "detail 7d:\n{text}");
        assert!(text.contains("sonnet 10%"), "detail sonnet:\n{text}");
        assert!(text.contains("real"), "source 标签:\n{text}");
    }

    #[test]
    fn fmt_local_hm_shape() {
        let s = fmt_local_hm(1_774_605_600_000);
        assert_eq!(s.len(), 5);
        assert!(s.as_bytes()[2] == b':' && s[..2].chars().all(|c| c.is_ascii_digit()));
    }
```

`src/render.rs` tests：既有调用全部改三参 `render_tmux(&sessions, now, None)`，新增：

```rust
    #[test]
    fn real_percent_chip_appended_when_present() {
        let sessions = vec![sess("%1", Some("api"), SessionState::Working, 0)];
        assert!(render_tmux(&sessions, 0, Some(62)).ends_with(" 62%"));
        assert!(!render_tmux(&sessions, 0, None).contains('%'));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test tui::view; cargo test render`
Expected: FAIL

- [ ] **Step 3: 实现**

- `src/tui/i18n.rs`：`Texts` 加字段（EN/ZH 双份）：`quota_real_source`（"real" / "真实"）、`quota_local_source` 若已有 source 标签字段则复用现有命名（读现文件为准，保持既有字段不动，只加缺的）；`quota_age_just`/`quota_age_min_fmt` 如详情年龄展示需要（"just now"/"刚刚"、"{n}m ago"/"{n}分钟前"）。
- `src/tui/view.rs`：
  - `header_line`：quota 条目 `matches!(q.source, QuotaSource::RealApi)` 且 `window_5h_percent.is_some()` 时输出 `format!("  ·  {} 5h {}%·7d {}%", q.provider.label(), p5, p7)`（p7 缺省显示 `--`）；否则维持既有 burn 文案。
  - 详情栏 quota 行改两行：真实态 → `5h 62% (resets 18:00) · 7d 31% (…) · sonnet 10% · real · 3m ago` 风格（sonnet 缺省省略；年龄 = `generated_at_ms.saturating_sub(freshness_ms)` 走既有 `fmt_duration`；重置时刻 `fmt_local_hm`）；本地估算态维持现文案并追加 burn（自 header 移入——若详情已有 burn 行则不重复）。
  - 新纯函数：

```rust
/// epoch ms → 本地时区 HH:MM（详情栏重置时刻）。libc 本地时——与 quiet_hours 同源做法。
pub fn fmt_local_hm(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm); }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}
```

- `src/render.rs`：`pub fn render_tmux(sessions: &[AgentSession], now_ms: u64, real_5h_pct: Option<u8>) -> String`——在最终 join 前 `if let Some(p) = real_5h_pct { parts.push(format!("{p}%")); }`（"tfa:idle" 空态分支同样追加）。
- `src/commands/status.rs`：`run` 里取快照后：

```rust
    let cfg = crate::config::Config::load();
    let pct = cfg.quota.status_bar_percent.then(|| {
        quota.iter().find(|q| matches!(q.source, crate::quota::QuotaSource::RealApi)
            && q.provider == crate::event::AgentKind::Claude)
            .and_then(|q| q.window_5h_percent)
    }).flatten();
```

传给 `render_tmux(&sessions, now, pct)`。其它 `render_tmux` 调用点（含测试）補 `None`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test` + clippy。Expected: 全绿零警告。

- [ ] **Step 5: Commit**

```bash
git add src/tui/view.rs src/tui/i18n.rs src/render.rs src/commands/status.rs
git commit -m "feat(quota): real percents in TUI header/detail + optional status-bar chip (i18n en/zh)"
```

---

### Task 6: quota_alert（迟滞告警 + notify 接入 + e2e + 文档）

**Files:**
- Modify: `src/notify/mod.rs`（NotifyKind::QuotaAlert）
- Modify: `src/notify/channels.rs`（tmux_send 空 pane_id 分支）
- Modify: `src/quota/real.rs`（AlertArm + fetcher 接入）
- Create: `tests/quota_real_e2e.rs`
- Modify: `README.md`、`README.zh-CN.md`、spec（注记）

**Interfaces:**
- Consumes: Task 3 fetcher 的 `_tx` 占位、Task 2 的 alert_5h/alert_7d。
- Produces: `NotifyKind::QuotaAlert`（as_str `"quota_alert"`）；`real::AlertArm`（Default；`pub fn evaluate(&mut self, alert_5h: u8, alert_7d: u8, u: &RealUsage) -> Vec<(NotifyKind, String, String)>` 返回 (kind, title, body) 列表——迟滞：≥阈值触发一次并解除武装，回落 < 阈值-5 重新武装；阈值 0=该窗关闭）。

- [ ] **Step 1: 写失败的迟滞测试**

`real.rs` tests 追加：

```rust
    fn usage_pct(five: f64, seven: f64) -> RealUsage {
        RealUsage { five_hour_pct: five, five_hour_resets_ms: 111, seven_day_pct: seven,
                    seven_day_resets_ms: 222, seven_day_sonnet_pct: None, fetched_at_ms: 0 }
    }

    #[test]
    fn alert_hysteresis_fires_once_and_rearms_below_band() {
        let mut arm = AlertArm::default();
        assert!(arm.evaluate(85, 90, &usage_pct(50.0, 50.0)).is_empty());
        let fired = arm.evaluate(85, 90, &usage_pct(87.0, 50.0));
        assert_eq!(fired.len(), 1, "5h 越过 85 触发一次");
        assert!(arm.evaluate(85, 90, &usage_pct(88.0, 50.0)).is_empty(), "武装解除期不再触发");
        assert!(arm.evaluate(85, 90, &usage_pct(83.0, 50.0)).is_empty(), "83 未低于 80，仍未武装");
        assert!(arm.evaluate(85, 90, &usage_pct(79.0, 50.0)).is_empty(), "回落 <80 重新武装（本轮不触发）");
        assert_eq!(arm.evaluate(85, 90, &usage_pct(86.0, 50.0)).len(), 1, "再次越过再触发");
    }

    #[test]
    fn alert_both_windows_and_zero_disables() {
        let mut arm = AlertArm::default();
        let fired = arm.evaluate(85, 90, &usage_pct(90.0, 95.0));
        assert_eq!(fired.len(), 2, "两窗同时越过各触发一条");
        let mut off = AlertArm::default();
        assert!(off.evaluate(0, 0, &usage_pct(99.0, 99.0)).is_empty(), "阈值 0=关");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test quota::real`
Expected: FAIL（AlertArm 不存在）

- [ ] **Step 3: 实现**

`src/notify/mod.rs`：`NotifyKind` 加 `QuotaAlert`，`as_str` 加 `Self::QuotaAlert => "quota_alert"`（编译器会带出 discipline/channels 的 match 完备性——只在 as_str/显示处补臂，Discipline 的会话状态映射不涉及该变体）。

`src/notify/channels.rs` `tmux_send`：pane_id 为空时不带 `-t`（quota 告警无 pane 语境）：

```rust
    if ev.pane_id.is_empty() {
        c.args(["display-message", &format!("[tfa] {}", ev.title)]);
    } else {
        c.args(["display-message", "-t", &ev.pane_id, &format!("[tfa] {}", ev.title)]);
    }
```

`real.rs` 追加：

```rust
/// quota_alert 迟滞（spec §9）：越过阈值触发一次即解除武装；回落到「阈值-5」
/// 以下重新武装（5h 窗每 5 小时自然重置也会经由回落路径重武装）。阈值 0=关。
#[derive(Default)]
pub struct AlertArm { fired_5h: bool, fired_7d: bool }
impl AlertArm {
    pub fn evaluate(&mut self, alert_5h: u8, alert_7d: u8, u: &RealUsage)
        -> Vec<(crate::notify::NotifyKind, String, String)> {
        let mut out = Vec::new();
        let mut window = |fired: &mut bool, threshold: u8, pct: f64, label: &str, resets_ms: u64| {
            if threshold == 0 { return; }
            let rearm = threshold.saturating_sub(5) as f64;
            if *fired && pct < rearm { *fired = false; }
            if !*fired && pct >= threshold as f64 {
                *fired = true;
                out.push((
                    crate::notify::NotifyKind::QuotaAlert,
                    format!("Claude {label} {}%", pct.round() as u8),
                    format!("resets {}", crate::tui::view::fmt_local_hm(resets_ms)),
                ));
            }
        };
        window(&mut self.fired_5h, alert_5h, u.five_hour_pct, "5h", u.five_hour_resets_ms);
        window(&mut self.fired_7d, alert_7d, u.seven_day_pct, "7d", u.seven_day_resets_ms);
        out
    }
}
```

fetcher `spawn` 循环接入：`let _tx = tx;` 删除，改持 `let mut arm = AlertArm::default();`；`FetchOutcome::Ok(u)` 分支写 cell 后：

```rust
                        for (kind, title, body) in arm.evaluate(q.alert_5h, q.alert_7d, &u) {
                            let _ = tx.send(crate::notify::NotifyEvent {
                                session_key: "quota:claude".into(),
                                pane_id: String::new(),
                                session_name: None,
                                kind, title, body,
                            });
                        }
```

新建 `tests/quota_real_e2e.rs`（沿用 notify_e2e 的 TFA_NO_NOTIFY sink 模式——读该文件对齐环境变量与守卫写法）：

```rust
//! 真实配额 e2e：默认关 = 零行为变化；notify-sink 无 quota_alert 事件。
//! （real=true 的正向链路依赖外网+凭证，属人工真机验收——spec §11。）
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn default_config_spawns_no_quota_fetcher_and_no_alerts() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    let daemon = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_CONFIG_PATH", dir.path().join("nonexistent.toml"))
        .env("TFA_NO_SCAN", "1").env("TFA_SKIP_TMUX_CHECK", "1").env("TFA_NO_NOTIFY", "1")
        .arg("daemon").spawn().unwrap();
    struct Guard(std::process::Child);
    impl Drop for Guard { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let _g = Guard(daemon);
    let start = Instant::now();
    while !sock.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
    }
    // 注入一个会话（触发 tick/快照管线），再取快照确认 quota 段仍是 local_estimate 语义
    let mut hook = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock).env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1").env("TMUX_PANE", "%1")
        .args(["hook", "claude", "user-prompt-submit"])
        .stdin(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write as _;
    hook.stdin.take().unwrap().write_all(b"{}").unwrap();
    assert!(hook.wait().unwrap().success());
    let out = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock).env("TFA_STATE_DIR", dir.path()).env("TFA_NO_SPAWN", "1")
        .arg("list").output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for q in json["quota"].as_array().unwrap() {
        assert_ne!(q["source"], "real_api", "默认关时绝不能出现 real_api");
    }
    // sink 里不得有 quota_alert
    let sink = std::fs::read_to_string(dir.path().join("notify-sink.jsonl")).unwrap_or_default();
    assert!(!sink.contains("quota_alert"), "sink: {sink}");
}
```

（依赖：dev-deps 已有 tempfile/serde_json 在 normal deps——`serde_json` 是主依赖可直接用。）

文档：README.md + README.zh-CN.md 的 Quota 章节各加「Real quota (opt-in)」小节：开关示例（spec §10 的 toml 原样）、风险声明一句（非官方接口、默认关、温和轮询）、alert 行为一句；spec §14 加一句 Task 4 的边界注记（real=true 但尚无 Claude 会话活动时 quota 段为空，属预期）。

- [ ] **Step 4: 跑全量 + e2e**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: 全绿零警告（新 e2e 在内）。

- [ ] **Step 5: Commit**

```bash
git add src/notify src/quota/real.rs tests/quota_real_e2e.rs README.md README.zh-CN.md docs/superpowers/specs/2026-07-13-tfa-real-quota-design.md
git commit -m "feat(quota): quota_alert hysteresis via notify pipeline + default-off e2e + docs"
```

---

## 真机验收清单（人工，用户执行）

1. `~/.config/tfa/config.toml` 写 `[quota] real = true` → 重启 daemon（pkill 自愈）→ 首次 Keychain 授权框点允许。
2. `tfa list` 出现 `"source":"real_api"` 与三窗口百分比；TUI header 变 `claude 5h N%·7d M%`，详情三窗口+重置时刻+年龄。
3. `status_bar_percent = true` → 状态栏出现 `N%` chip。
4. 阈值临时调低（如 `alert_5h = 1`）→ 下轮拉取收到一次通知，且不重复轰炸。
5. 断网/改错 token 场景：30 分钟内显示旧值+年龄增长，过后回落 `≥N tokens` 本地估算。
6. 删掉 `real = true` → 行为完全回到现状。

## Self-Review 记录

- **Spec 覆盖**：§3 事实→T1 fixture/解析；§4 架构→T3/T4；§5 数据→T4；§6 纪律→T2（钳制）/T3（退避/单飞/429/401）；§7 降级→T4（TTL 粘滞测试）；§8 展示→T5；§9 告警→T6；§10 config→T2；§11 测试→各任务+T6 e2e；§12 授权门→T1 派单流程；§13 任务分解一一对应；§14 限制→T4 边界注记（T6 落文档）。
- **占位符扫描**：无 TBD；T5 的 i18n 字段名以现文件为准属「读后适配」指令而非占位（i18n.rs 现字段名 plan 作者未逐一复核，实施者按现文件对齐）。
- **类型一致性**：`RealUsage`/`FetchOutcome`/`RealQuotaCell`/`Backoff`/`AlertArm`/`merge`/`effective_refresh_secs` 签名跨任务引用一致；`render_tmux` 三参签名 T5 内闭环；`NotifyKind::QuotaAlert` T6 定义并消费。
