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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyConfig;
    use crate::event::AgentKind;
    use crate::state::{AgentSession, SessionState, Source};

    fn sess(key: &str, state: SessionState) -> AgentSession {
        AgentSession {
            pane_id: key.into(), agent: AgentKind::Claude, session_name: Some("api".into()),
            state, state_since_ms: 0, current_task: None, cwd: None, last_activity_ms: 0,
            source: Source::Hook, pid: None, model: None, context: None, tokens: None,
            git_branch: None, transcript_path: None, agent_session_id: Some(key.into()), consumed_tokens: 0,
        }
    }
    #[allow(clippy::field_reassign_with_default)] // brief's exact test fixture shape
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
