use crate::event::{AgentEvent, AgentKind, EventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEAD_RETENTION_MS: u64 = 300_000;
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

#[derive(Default, Serialize, Deserialize)]
pub struct StateStore {
    sessions: BTreeMap<String, AgentSession>,
}

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
    #[allow(dead_code)] // documented ctor in the M1 interface; production call site now uses unwrap_or_default()
    pub fn new() -> Self { Self::default() }

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
            // 只刷新 last_activity；但 Dead 条目收到任何 hook 流量都是存活证据，必须复活。
            EventKind::Activity => {
                if matches!(entry.state, SessionState::Dead) { Some(SessionState::Working) } else { None }
            }
        };
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

    pub fn reconcile_liveness(&mut self, live: &BTreeMap<String, Option<u32>>, now_ms: u64) {
        for s in self.sessions.values_mut() {
            if matches!(s.state, SessionState::Dead) {
                // 同 pane 同 kind 的 agent 不经 hook 重启：scan 看到活进程即复活，
                // 否则要等 DEAD_RETENTION_MS(5min) 才会被 prune 重建，期间是 ghost-dead。
                //
                // Narrow race, intentional: a genuine SessionEnd hook can mark
                // this Dead in the same window a scan tick observes the old
                // pid still alive (process hasn't exited yet from the OS's
                // point of view) — this revive branch flips it back to
                // Working for at most one scan cycle before the next tick (or
                // the process actually exiting) corrects it again. Accepted:
                // self-heals within one cycle, and the alternative (delaying
                // revive) reintroduces the ghost-dead window this branch
                // exists to close.
                if let Some(Some(pid)) = live.get(&s.pane_id) {
                    s.state = SessionState::Working;
                    s.state_since_ms = now_ms;
                    s.last_activity_ms = now_ms;
                    s.pid = Some(*pid);
                }
                continue;
            }
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

    /// Updates whichever `Some` fields differ from what's stored. A scan-only
    /// session (e.g. codex — its hooks aren't wired to tfa) never gets an
    /// `Activity` hook, so without this it goes permanently Stale after
    /// STALE_AFTER_MS even while genuinely working: metrics keep updating
    /// but the state freezes (spec principle: 文件赢事实 — changed transcript/db
    /// facts are proof of life). So: any field that actually *changes* bumps
    /// `last_activity_ms` and un-Stales the session; re-reading identical
    /// metrics (e.g. codex polling an idle thread) must NOT count as
    /// activity, or an idle scan-only session would never go Stale at all.
    pub fn set_metrics(&mut self, pane_id: &str, m: SessionMetrics, now_ms: u64) {
        if let Some(s) = self.sessions.get_mut(pane_id) {
            let mut changed = false;
            if m.model.is_some() && m.model != s.model { s.model = m.model; changed = true; }
            if m.context.is_some() && m.context != s.context { s.context = m.context; changed = true; }
            if m.tokens.is_some() && m.tokens != s.tokens { s.tokens = m.tokens; changed = true; }
            if m.git_branch.is_some() && m.git_branch != s.git_branch { s.git_branch = m.git_branch; changed = true; }
            if changed {
                s.last_activity_ms = now_ms;
                if matches!(s.state, SessionState::Stale) {
                    s.state = SessionState::Working;
                    s.state_since_ms = now_ms;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentKind, EventKind};

    fn ev(kind: EventKind, at_ms: u64) -> AgentEvent {
        AgentEvent {
            agent: AgentKind::Claude, pane_id: "%1".into(), kind,
            reason: None, prompt: None, cwd: None, at_ms,
            transcript_path: None, session_id: None,
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
    fn activity_event_revives_dead_entry() {
        // 任何 hook 流量都是存活证据：一个被短暂 ps 失败误判 Dead 的 working claude
        // 不该等到下一个非 activity 事件才复活。
        let mut st = StateStore::new();
        st.apply(ev(EventKind::SessionEnd, 1000));
        st.apply(ev(EventKind::Activity, 2000));
        let s = &st.sessions()[0];
        assert!(matches!(s.state, SessionState::Working));
        assert_eq!(s.state_since_ms, 2000);
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

    #[test]
    fn waiting_input_session_serializes_flattened() {
        // Task 7 渲染和 Task 9 e2e 都依赖这个扁平形状：
        // {"pane_id":"%1","state":"waiting_input","reason":"perm",...}
        let sess = AgentSession {
            pane_id: "%1".into(),
            agent: AgentKind::Claude,
            session_name: None,
            state: SessionState::WaitingInput { reason: "perm".into() },
            state_since_ms: 100,
            current_task: None,
            cwd: None,
            last_activity_ms: 100,
            source: Source::Hook,
            pid: None,
            model: None,
            context: None,
            tokens: None,
            git_branch: None,
            transcript_path: None,
            agent_session_id: None,
        };
        let json = serde_json::to_string(&sess).unwrap();
        assert!(json.contains(r#""state":"waiting_input""#), "json was: {json}");
        assert!(json.contains(r#""reason":"perm""#), "json was: {json}");
        assert!(json.contains(r#""pane_id""#), "json was: {json}");
    }

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
        st.set_metrics("%9", m, 1000);
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
        }, 1000);
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
    fn reconcile_liveness_leaves_dead_alone_without_live_process() {
        // Dead + 无存活进程（Some(None) 或 pane 缺席）：保持 Dead，state_since 不重置
        // （否则 prune 永不触发）。区别于下面 revive 用例：那个用例是 Dead + Some(pid)。
        let mut st = StateStore::new();
        st.apply(ev(EventKind::SessionEnd, 1000)); // 已 Dead
        let mut live = std::collections::BTreeMap::new();
        live.insert("%1".to_string(), None::<u32>);
        st.reconcile_liveness(&live, 5000);
        let s = &st.sessions()[0];
        assert!(matches!(s.state, SessionState::Dead));
        assert_eq!(s.state_since_ms, 1000);
    }

    #[test]
    fn reconcile_liveness_revives_dead_with_live_process() {
        // 同 kind agent 在同一 pane 里不经 hook 重启：scan 看到活进程必须复活该条目，
        // 否则 ghost-dead 状态要等 5min DEAD_RETENTION_MS 才会被 prune 重建。
        let mut st = StateStore::new();
        st.apply(ev(EventKind::SessionEnd, 1000)); // %1 Dead
        let mut live = std::collections::BTreeMap::new();
        live.insert("%1".to_string(), Some(77u32));
        st.reconcile_liveness(&live, 5000);
        let s = &st.sessions()[0];
        assert!(matches!(s.state, SessionState::Working), "live process must revive Dead entry");
        assert_eq!(s.state_since_ms, 5000);
        assert_eq!(s.pid, Some(77));
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
    fn changed_metrics_bump_activity_and_unstale() {
        // 纯扫描会话（如 codex：hook 没接进 tfa）只靠 metrics 变化证明存活 ——
        // “文件赢事实”：transcript/db 里指标真的变了，就是活的证据，必须把
        // Stale 打回 Working，否则它 300s 后永久卡死在 Stale（F2）。
        let mut st = StateStore::new();
        st.upsert_scanned("%1", AgentKind::Codex, 7, "/p", "s", 1000);
        st.stale_sweep(1000 + STALE_AFTER_MS + 1); // Stale
        st.set_metrics("%1", SessionMetrics {
            model: Some("gpt-5.3-codex".into()),
            tokens: Some(TokenTotals { total: 500, ..Default::default() }),
            context: None,
            git_branch: None,
        }, 400_000);
        let s = &st.sessions()[0];
        assert!(matches!(s.state, SessionState::Working), "changing metrics must un-Stale");
        assert_eq!(s.last_activity_ms, 400_000);
    }

    #[test]
    fn identical_metrics_do_not_bump_activity() {
        let mut st = StateStore::new();
        st.upsert_scanned("%1", AgentKind::Codex, 7, "/p", "s", 1000);
        let m = SessionMetrics {
            model: Some("gpt-5.3-codex".into()),
            tokens: Some(TokenTotals { total: 500, ..Default::default() }),
            context: None,
            git_branch: None,
        };
        st.set_metrics("%1", m.clone(), 2000);
        let after_first = st.sessions()[0].last_activity_ms;
        st.set_metrics("%1", m, 9000);
        assert_eq!(st.sessions()[0].last_activity_ms, after_first, "unchanged metrics must not count as activity");
        st.stale_sweep(after_first + STALE_AFTER_MS + 1);
        assert!(matches!(st.sessions()[0].state, SessionState::Stale), "idle scan-only session must still go Stale");
    }

    #[test]
    fn panes_needing_name_lists_unnamed_only() {
        let mut st = StateStore::new();
        st.apply(ev(EventKind::SessionStart, 0));                       // %1 无名
        st.upsert_scanned("%2", AgentKind::Claude, 7, "/p", "named", 0); // %2 有名
        assert_eq!(st.panes_needing_name(), vec!["%1".to_string()]);
    }
}
