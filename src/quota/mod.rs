pub mod burn;
pub mod real;

use crate::event::AgentKind;
use burn::BurnSampler;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const WINDOW_5H_MS: u64 = 5 * 3_600_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource { LocalEstimate }

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for QuotaCache {
    fn default() -> Self { Self::new() }
}

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
