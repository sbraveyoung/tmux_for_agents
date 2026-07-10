//! 纯逻辑，无 IO / 无终端：最新快照 + 连接态 + 按键处理。
//! Task 3 在此追加：排序、pane_id 选中延续、Enter 导航。

use crate::quota::QuotaState;
use crate::state::AgentSession;
use crate::tui::poll::PollMsg;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Action {
    None,
    Redraw,
    Quit,
}

pub struct Model {
    pub sessions: Vec<AgentSession>,
    pub quota: Vec<QuotaState>,
    pub generated_at_ms: u64,
    pub connected: bool,
}

impl Model {
    pub fn new() -> Self {
        Self { sessions: vec![], quota: vec![], generated_at_ms: 0, connected: false }
    }

    /// 返回值 = 是否需要重绘（draw-on-change 的依据）。
    pub fn apply_msg(&mut self, msg: PollMsg) -> bool {
        match msg {
            PollMsg::Snapshot { sessions, quota, generated_at_ms } => {
                self.sessions = sessions;
                self.quota = quota;
                self.generated_at_ms = generated_at_ms;
                self.connected = true;
                true
            }
            PollMsg::Disconnected => {
                let changed = self.connected;
                self.connected = false;
                changed
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_sets_connected_and_requests_redraw() {
        let mut m = Model::new();
        assert!(!m.connected);
        let redraw = m.apply_msg(PollMsg::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 7 });
        assert!(redraw && m.connected && m.generated_at_ms == 7);
    }

    #[test]
    fn repeated_disconnected_redraws_only_once() {
        let mut m = Model::new();
        m.apply_msg(PollMsg::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 1 });
        assert!(m.apply_msg(PollMsg::Disconnected), "first disconnect must redraw");
        assert!(!m.apply_msg(PollMsg::Disconnected), "repeat disconnect must not redraw");
    }

    #[test]
    fn quit_keys() {
        let mut m = Model::new();
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
}
