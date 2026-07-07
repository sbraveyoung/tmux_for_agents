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
