//! 纯逻辑，无 IO / 无终端：最新快照 + 选中(pane_id) + 连接态 + 按键处理。
//!
//! 选中延续按 pane_id 而非列表下标：1s 刷新间列表会增删重排，按下标会
//! 「看着选 A、Enter 跳到 B 的 pane」——Enter 改变物理焦点，选错代价高（spec §6）。

use crate::quota::QuotaState;
use crate::state::{AgentSession, SessionState};
use crate::tui::poll::PollMsg;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Action {
    None,
    Redraw,
    Quit,
    /// Enter：跳转到该 pane_id（执行在 commands/tui.rs + nav.rs）
    Navigate(String),
}

pub struct Model {
    pub sessions: Vec<AgentSession>,
    pub quota: Vec<QuotaState>,
    pub generated_at_ms: u64,
    pub connected: bool,
    /// 当前选中会话的 pane_id（全局唯一，如 "%37"）
    pub selected: Option<String>,
    /// 导航失败/不可用时的一行提示（Footer 显示；新快照到达即清）
    pub nav_error: Option<String>,
    pub in_tmux: bool,
}

/// 状态紧急度（spec §6）：waiting < working < starting < done < stale < dead
pub fn state_rank(s: &SessionState) -> u8 {
    match s {
        SessionState::WaitingInput { .. } => 0,
        SessionState::Working => 1,
        SessionState::Starting => 2,
        SessionState::Done => 3,
        SessionState::Stale => 4,
        SessionState::Dead => 5,
    }
}

pub fn sort_sessions(sessions: &mut [AgentSession]) {
    sessions.sort_by(|a, b| {
        state_rank(&a.state).cmp(&state_rank(&b.state)).then_with(|| {
            if matches!(a.state, SessionState::WaitingInput { .. }) {
                a.state_since_ms.cmp(&b.state_since_ms) // 等最久的浮最顶
            } else {
                b.last_activity_ms.cmp(&a.last_activity_ms) // 最近活跃在前
            }
        })
    });
}

/// 新快照后重定位光标：pane_id 还在 → 跟着走；消失 → clamp 到原下标附近；空列表 → None。
pub fn reselect(old: Option<&str>, old_index: Option<usize>, list: &[AgentSession]) -> Option<String> {
    if list.is_empty() {
        return None;
    }
    if let Some(pane) = old {
        if list.iter().any(|s| s.pane_id == pane) {
            return Some(pane.to_string());
        }
    }
    let idx = old_index.unwrap_or(0).min(list.len() - 1);
    Some(list[idx].pane_id.clone())
}

impl Model {
    pub fn new(in_tmux: bool) -> Self {
        Self {
            sessions: vec![],
            quota: vec![],
            generated_at_ms: 0,
            connected: false,
            selected: None,
            nav_error: None,
            in_tmux,
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let pane = self.selected.as_deref()?;
        self.sessions.iter().position(|s| s.pane_id == pane)
    }

    pub fn selected_session(&self) -> Option<&AgentSession> {
        self.selected_index().map(|i| &self.sessions[i])
    }

    /// 返回值 = 是否需要重绘（draw-on-change 的依据）。
    pub fn apply_msg(&mut self, msg: PollMsg) -> bool {
        match msg {
            PollMsg::Snapshot { mut sessions, quota, generated_at_ms } => {
                sort_sessions(&mut sessions);
                let old_index = self.selected_index();
                self.selected = reselect(self.selected.as_deref(), old_index, &sessions);
                self.sessions = sessions;
                self.quota = quota;
                self.generated_at_ms = generated_at_ms;
                self.connected = true;
                self.nav_error = None; // 下一次快照自然纠正（spec §7.3）
                true
            }
            PollMsg::Disconnected => {
                let changed = self.connected;
                self.connected = false;
                changed
            }
        }
    }

    fn move_selection(&mut self, delta: i32) -> Action {
        if self.sessions.is_empty() {
            return Action::None;
        }
        let cur = self.selected_index().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, self.sessions.len() as i32 - 1) as usize;
        let pane = self.sessions[next].pane_id.clone();
        if self.selected.as_deref() == Some(pane.as_str()) {
            return Action::None; // 边界不动：不重绘
        }
        self.selected = Some(pane);
        Action::Redraw
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter => {
                if !self.in_tmux {
                    self.nav_error = Some("非 tmux 环境，跳转不可用".into());
                    return Action::Redraw;
                }
                match &self.selected {
                    Some(pane) => Action::Navigate(pane.clone()),
                    None => Action::None,
                }
            }
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;
    use crate::state::{SessionState, Source};

    fn sess(pane: &str, state: SessionState, since: u64, last: u64) -> AgentSession {
        AgentSession {
            pane_id: pane.into(),
            agent: AgentKind::Claude,
            session_name: None,
            state,
            state_since_ms: since,
            current_task: None,
            cwd: None,
            last_activity_ms: last,
            source: Source::Hook,
            pid: None,
            model: None,
            context: None,
            tokens: None,
            git_branch: None,
            transcript_path: None,
            agent_session_id: None,
            consumed_tokens: 0,
        }
    }

    fn waiting(reason: &str) -> SessionState {
        SessionState::WaitingInput { reason: reason.into() }
    }

    #[test]
    fn snapshot_sets_connected_and_requests_redraw() {
        let mut m = Model::new(true);
        assert!(!m.connected);
        let redraw = m.apply_msg(PollMsg::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 7 });
        assert!(redraw && m.connected && m.generated_at_ms == 7);
    }

    #[test]
    fn repeated_disconnected_redraws_only_once() {
        let mut m = Model::new(true);
        m.apply_msg(PollMsg::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 1 });
        assert!(m.apply_msg(PollMsg::Disconnected), "first disconnect must redraw");
        assert!(!m.apply_msg(PollMsg::Disconnected), "repeat disconnect must not redraw");
    }

    #[test]
    fn quit_keys() {
        let mut m = Model::new(true);
        for key in [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(m.handle_key(key), Action::Quit));
        }
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::None
        ));
    }

    #[test]
    fn sort_urgency_then_group_order() {
        // waiting(等最久浮顶) < working(最近活跃在前) < starting < done < stale < dead
        let mut v = vec![
            sess("%1", SessionState::Working, 0, 500),
            sess("%2", waiting("a"), 300, 300),
            sess("%3", waiting("b"), 100, 100),   // 等更久（since 更小）→ 排 %2 前
            sess("%4", SessionState::Dead, 0, 900),
            sess("%5", SessionState::Done, 0, 50),
            sess("%6", SessionState::Working, 0, 900), // 比 %1 更近活跃 → 排 %1 前
        ];
        sort_sessions(&mut v);
        let ids: Vec<_> = v.iter().map(|s| s.pane_id.as_str()).collect();
        assert_eq!(ids, ["%3", "%2", "%6", "%1", "%5", "%4"]);
    }

    #[test]
    fn reselect_keeps_pane_if_still_present() {
        let list = vec![sess("%1", SessionState::Working, 0, 0), sess("%2", SessionState::Done, 0, 0)];
        assert_eq!(reselect(Some("%2"), Some(0), &list), Some("%2".into()));
    }

    #[test]
    fn reselect_clamps_to_old_index_when_pane_gone() {
        // 旧选中 %9 已消失，旧下标 5 超界 → clamp 到新列表末行
        let list = vec![sess("%1", SessionState::Working, 0, 0), sess("%2", SessionState::Done, 0, 0)];
        assert_eq!(reselect(Some("%9"), Some(5), &list), Some("%2".into()));
        // 旧下标 0 → 新列表第 0 行
        assert_eq!(reselect(Some("%9"), Some(0), &list), Some("%1".into()));
    }

    #[test]
    fn reselect_empty_list_is_none_and_default_is_first() {
        assert_eq!(reselect(Some("%1"), Some(0), &[]), None);
        let list = vec![sess("%8", SessionState::Working, 0, 0)];
        assert_eq!(reselect(None, None, &list), Some("%8".into()));
    }

    #[test]
    fn arrows_move_selection_and_clamp_at_edges() {
        let mut m = Model::new(true);
        m.apply_msg(PollMsg::Snapshot {
            sessions: vec![sess("%1", waiting("x"), 0, 0), sess("%2", SessionState::Working, 0, 9)],
            quota: vec![],
            generated_at_ms: 10,
        });
        assert_eq!(m.selected.as_deref(), Some("%1"), "首个快照默认选中第一行");
        assert!(matches!(m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)), Action::Redraw));
        assert_eq!(m.selected.as_deref(), Some("%2"));
        // 底部再往下 → 不动，不重绘
        assert!(matches!(m.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)), Action::None));
        assert!(matches!(m.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)), Action::Redraw));
        assert_eq!(m.selected.as_deref(), Some("%1"));
        assert!(matches!(m.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)), Action::None));
    }

    #[test]
    fn selection_follows_pane_id_across_resort() {
        // 防「看着选 A、Enter 跳到 B」：选中存 pane_id，重排后跟着走
        let mut m = Model::new(true);
        m.apply_msg(PollMsg::Snapshot {
            sessions: vec![sess("%1", SessionState::Working, 0, 5), sess("%2", SessionState::Working, 0, 1)],
            quota: vec![],
            generated_at_ms: 10,
        });
        m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // 选中 %2（第 2 行）
        // 新快照：%2 变 waiting → 浮到第 1 行
        m.apply_msg(PollMsg::Snapshot {
            sessions: vec![sess("%1", SessionState::Working, 0, 5), sess("%2", waiting("x"), 6, 6)],
            quota: vec![],
            generated_at_ms: 20,
        });
        assert_eq!(m.selected.as_deref(), Some("%2"), "选中必须按 pane_id 延续");
        assert_eq!(m.selected_index(), Some(0), "%2 已浮顶");
    }

    #[test]
    fn enter_outside_tmux_sets_error_inside_tmux_navigates() {
        let list = vec![sess("%1", waiting("x"), 0, 0)];
        let mut out_tmux = Model::new(false);
        out_tmux.apply_msg(PollMsg::Snapshot { sessions: list.clone(), quota: vec![], generated_at_ms: 1 });
        assert!(matches!(out_tmux.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::Redraw));
        assert_eq!(out_tmux.nav_error.as_deref(), Some("非 tmux 环境，跳转不可用"));

        let mut in_tmux = Model::new(true);
        in_tmux.apply_msg(PollMsg::Snapshot { sessions: list, quota: vec![], generated_at_ms: 1 });
        match in_tmux.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Navigate(pane) => assert_eq!(pane, "%1"),
            _ => panic!("expected Navigate"),
        }
        // 空列表 Enter → None
        let mut empty = Model::new(true);
        assert!(matches!(empty.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::None));
    }

    #[test]
    fn new_snapshot_clears_nav_error() {
        let mut m = Model::new(false);
        m.apply_msg(PollMsg::Snapshot { sessions: vec![sess("%1", waiting("x"), 0, 0)], quota: vec![], generated_at_ms: 1 });
        m.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(m.nav_error.is_some());
        m.apply_msg(PollMsg::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 2 });
        assert!(m.nav_error.is_none(), "新快照自然纠正错误提示（spec §7.3）");
    }
}
