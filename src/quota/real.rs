//! 真实配额（spec: docs/superpowers/specs/2026-07-13-tfa-real-quota-design.md）。
//! 本文件承载与非官方 oauth/usage 接口相关的全部逻辑；`config.quota.real=false`
//! （默认）时本模块除类型定义外的任何代码都不会被 daemon 触达。

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // no production consumer yet; Task 3 fetcher constructs it, Task 4 snapshot-merge consumes it
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
#[allow(dead_code)] // no production consumer yet; called by parse_iso8601_ms, which Task 3 fetcher wires in
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
#[allow(dead_code)] // no production consumer yet; Task 3 fetcher calls this to parse resets_at
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
#[allow(dead_code)] // no production consumer yet; Task 3 fetcher calls this on each poll response
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
    fn usage_response_rejects_malformed() {
        assert!(parse_usage_response("not json", 0).is_none());
        assert!(parse_usage_response(r#"{"five_hour":{}}"#, 0).is_none());
    }
}
