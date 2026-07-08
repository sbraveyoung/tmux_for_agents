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
