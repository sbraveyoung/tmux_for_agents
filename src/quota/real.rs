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
    if !(1..=12).contains(&mo) || !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&sec) { return None; }
    // 日历严格：日分量必须真实存在于该年月，否则 days_from_civil 会静默滚动进位（2026-02-31 → 3 月 3 日）。
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month: u32 = match mo {
        2 => if leap { 29 } else { 28 },
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days_in_month).contains(&d) { return None; }
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
            if !(0..=23).contains(&oh) || !(0..=59).contains(&om) { return None; }
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

/// fetcher 线程（spec §4）：仅 config.quota.real=true 时由 daemon spawn。
/// 严格单飞：线程内串行 loop。
pub fn spawn(
    cell: std::sync::Arc<std::sync::Mutex<RealQuotaCell>>,
    cfg: std::sync::Arc<std::sync::Mutex<crate::config::Config>>,
    tx: std::sync::mpsc::Sender<crate::notify::NotifyEvent>,
) {
    std::thread::spawn(move || {
        let mut arm = AlertArm::default();
        let mut token = read_credentials();
        let mut backoff = Backoff::new();
        loop {
            // 每轮 config 快照：quota（阈值/间隔）+ notify（quiet_hours 静默门，见 Ok 分支）。
            let (q, ncfg) = {
                let c = cfg.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                (c.quota.clone(), c.notify.clone())
            };
            if !q.real { return; } // 关=零 API 不变量的防未来保险：今天 config 不会热更（spawn 门已足够），但任何未来的热重载路径都不该让在跑的 fetcher 违约（review 2026-07-14 建议）
            let base = effective_refresh_secs(&q);
            let sleep_secs; // 每条路径都恰好赋值一次（见各 match 分支）；初值无意义故不预置，避免死存储警告
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
                        // 先评估告警（借用 &u），再把 u 移入 cell——顺序对调是 `u` 被
                        // move 进 cell 后不能再借用的编译必然结果，不影响语义：evaluate
                        // 与写 cell 之间没有可观察的先后依赖（arm 只被本线程访问，
                        // 两步都在下一次 sleep 前完成）。
                        // quiet_hours 门（spec §9：quota_alert 不豁免；决策复用
                        // discipline::quiet_suppresses，不第二处实现解析/跨夜/豁免逻辑）。
                        // 静默期整体跳过评估（连武装状态都不动）：阈值在静默期被越过时，
                        // 静默结束后的下一轮拉取仍会触发补发——比 dispatch 侧丢弃更符合
                        // §9 意图（2026-07-14 实现注记）。
                        let quiet = crate::notify::discipline::quiet_suppresses(
                            &ncfg, crate::notify::NotifyKind::QuotaAlert, crate::daemon::now_ms());
                        if !quiet {
                            for (kind, title, body) in arm.evaluate(q.alert_5h, q.alert_7d, &u) {
                                let _ = tx.send(crate::notify::NotifyEvent {
                                    session_key: "quota:claude".into(),
                                    pane_id: String::new(),
                                    session_name: None,
                                    kind, title, body,
                                });
                            }
                        }
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

/// 真实值粘滞 TTL（spec §7）：Instant 单调钟计龄，过期即回落本地估算。
pub const TTL_SECS: u64 = 1800;

/// 快照组装处的合并（spec §5）：本地估算 + 新鲜真实值叠加。TTL 内才覆盖 Claude 条目的
/// 百分比/重置时间/来源/新鲜度；`observed_tokens_this_window`/`burn_rate_per_min` 永远
/// 保留本地观测——真实接口不提供这两个量，覆盖会丢失信息。过期或 cell 为空均原样返回
/// local（诚实：绝不用陈旧真实值冒充新鲜）。
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
        // 负偏移：本地 02:00 @ -08:00 = UTC 10:00 → 与 10:00Z 同一时刻
        assert_eq!(parse_iso8601_ms("2026-03-27T02:00:00-08:00"), Some(1_774_605_600_000));
        // 毫秒小数容忍
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00.500Z"), Some(1_774_605_600_500));
        // 闰年 2 月
        assert_eq!(parse_iso8601_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        // 垃圾输入
        assert_eq!(parse_iso8601_ms("not a date"), None);
        assert_eq!(parse_iso8601_ms("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn iso8601_rejects_calendar_impossible_forms() {
        // 不存在的日子必须 None，不得经 days_from_civil 滚动进位（2026-02-31 ≠ 3 月 3 日）
        assert_eq!(parse_iso8601_ms("2026-02-31T00:00:00Z"), None);
        // 30 天月同理（4 月无 31 日）
        assert_eq!(parse_iso8601_ms("2026-04-31T00:00:00Z"), None);
        // 负的子日分量不得当借位解析（10:-1 ≠ 09:59）
        assert_eq!(parse_iso8601_ms("2026-03-27T10:-1:00Z"), None);
        // 时区偏移分量越界（99:99）必须 None
        assert_eq!(parse_iso8601_ms("2026-03-27T10:00:00+99:99"), None);
    }

    #[test]
    fn usage_response_parses_fixture_shape() {
        let body = include_str!("../../fixtures/oauth_usage.json");
        let u = parse_usage_response(body, 42).expect("fixture parses");
        assert!(u.five_hour_pct >= 0.0 && u.five_hour_pct <= 100.0);
        assert!(u.five_hour_resets_ms > 1_500_000_000_000, "resets_at 是合理的未来 epoch ms");
        assert!(u.seven_day_resets_ms >= u.five_hour_resets_ms, "7d 重置不早于 5h");
        assert_eq!(u.fetched_at_ms, 42);
        // 真机核对注记（spec §3.2）：本账户 seven_day_sonnet 为 JSON null，须视同缺席 → None。
        assert_eq!(u.seven_day_sonnet_pct, None);
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
    fn usage_response_reads_present_sonnet_window() {
        let body = r#"{"five_hour":{"utilization":18.0,"resets_at":"2026-03-27T10:00:00+00:00"},
                       "seven_day":{"utilization":17.0,"resets_at":"2026-04-02T13:00:00+00:00"},
                       "seven_day_sonnet":{"utilization":10.0,"resets_at":"2026-04-02T13:00:00+00:00"}}"#;
        let u = parse_usage_response(body, 0).expect("parses with sonnet");
        let sonnet = u.seven_day_sonnet_pct.expect("sonnet 窗在场且非 null → Some");
        assert!((sonnet - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_response_rejects_malformed() {
        assert!(parse_usage_response("not json", 0).is_none());
        assert!(parse_usage_response(r#"{"five_hour":{}}"#, 0).is_none());
    }

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
    #[allow(clippy::field_reassign_with_default)] // brief's exact test fixture shape
    fn refresh_secs_clamped_to_300_floor() {
        let mut q = crate::config::QuotaConfig::default();
        q.refresh_secs = 60;
        assert_eq!(effective_refresh_secs(&q), 300);
        q.refresh_secs = 900;
        assert_eq!(effective_refresh_secs(&q), 900);
    }

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
}
