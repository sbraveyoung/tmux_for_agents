use crate::state::{AgentSession, SessionState};

/// `real_5h_pct`：真实配额任务（2026-07-14）新增——`[quota] status_bar_percent`
/// 开启且快照里有一条 `RealApi` 来源的 Claude 配额时，调用方（`commands::status::run`）
/// 传 `Some(该 provider 的 5h 真实百分比)`；否则传 `None`，渲染行为与今天完全一致
/// （不出现任何 '%' 字符）——纯展示开关，不影响 working/waiting/done 计数或
/// oldest-waiting 逻辑。
pub fn render_tmux(sessions: &[AgentSession], now_ms: u64, real_5h_pct: Option<u8>) -> String {
    let mut working = 0u32;
    let mut waiting = 0u32;
    let mut done = 0u32;
    let mut oldest_wait: Option<&AgentSession> = None;

    for s in sessions {
        match &s.state {
            SessionState::Working | SessionState::Starting => working += 1,
            SessionState::WaitingInput { .. } => {
                waiting += 1;
                if oldest_wait.is_none_or(|o| s.state_since_ms < o.state_since_ms) {
                    oldest_wait = Some(s);
                }
            }
            SessionState::Done => done += 1,
            SessionState::Dead | SessionState::Stale => {}
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if working > 0 { parts.push(format!("⚡{working}")); }
    if waiting > 0 { parts.push(format!("⏸{waiting}")); }
    if done > 0 { parts.push(format!("✓{done}")); }
    if parts.is_empty() {
        let mut idle = String::from("tfa:idle");
        if let Some(p) = real_5h_pct {
            idle.push_str(&format!(" {p}%"));
        }
        return idle;
    }

    if let Some(w) = oldest_wait {
        let mins = now_ms.saturating_sub(w.state_since_ms) / 60_000;
        let name = w.session_name.as_deref().unwrap_or(&w.pane_id);
        parts.push(format!("⏸{} {mins}m", truncate_name(name)));
    }
    if let Some(p) = real_5h_pct {
        parts.push(format!("{p}%"));
    }
    parts.join(" ")
}

/// Cap the displayed session name so one long worktree name can't flood
/// the tmux status bar (status-right shares space with other segments).
const NAME_MAX_CHARS: usize = 16;

fn truncate_name(name: &str) -> String {
    if name.chars().count() <= NAME_MAX_CHARS {
        name.to_string()
    } else {
        let head: String = name.chars().take(NAME_MAX_CHARS).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;
    use crate::state::{AgentSession, SessionState};

    fn sess(pane: &str, name: Option<&str>, state: SessionState, since: u64) -> AgentSession {
        AgentSession {
            pane_id: pane.into(), agent: AgentKind::Claude,
            session_name: name.map(String::from), state,
            state_since_ms: since, current_task: None, cwd: None, last_activity_ms: since,
            source: crate::state::Source::Hook,
            pid: None,
            model: None,
            context: None,
            tokens: None,
            git_branch: None,
            transcript_path: None,
            agent_session_id: None,
            consumed_tokens: 0,
            window_index: None,
            pane_index: None,
        }
    }

    #[test]
    fn empty_renders_idle() {
        assert_eq!(render_tmux(&[], 0, None), "tfa:idle");
    }

    #[test]
    fn counts_and_oldest_waiting_shown() {
        let sessions = vec![
            sess("%1", Some("api"), SessionState::Working, 0),
            sess("%2", Some("web"), SessionState::WaitingInput { reason: "perm".into() }, 60_000),
            sess("%3", None, SessionState::WaitingInput { reason: "input".into() }, 240_000),
            sess("%4", Some("done1"), SessionState::Done, 0),
        ];
        // now = 300s；%2 等了 4min（最久）
        let line = render_tmux(&sessions, 300_000, None);
        assert_eq!(line, "⚡1 ⏸2 ✓1 ⏸web 4m");
    }

    #[test]
    fn dead_and_zero_counts_omitted() {
        let sessions = vec![sess("%1", None, SessionState::Dead, 0)];
        assert_eq!(render_tmux(&sessions, 0, None), "tfa:idle");
    }

    #[test]
    fn long_session_name_is_truncated_with_ellipsis() {
        let sessions = vec![sess(
            "%1",
            Some("subscription_worktree-bold-fox-363m"),
            SessionState::WaitingInput { reason: "perm".into() },
            0,
        )];
        // 16 chars kept + ellipsis, so the status bar can't be flooded
        assert_eq!(render_tmux(&sessions, 60_000, None), "⏸1 ⏸subscription_wor… 1m");
    }

    #[test]
    fn short_session_name_is_untouched() {
        let sessions = vec![sess(
            "%1",
            Some("api"),
            SessionState::WaitingInput { reason: "perm".into() },
            0,
        )];
        assert_eq!(render_tmux(&sessions, 0, None), "⏸1 ⏸api 0m");
    }

    /// idle 分支同样追加 chip（brief 明确点名的边界）：working/waiting/done 全 0
    /// 时今天恒 "tfa:idle"，real_5h_pct 有值时必须在后面追加百分比，不能因为落进
    /// 了 idle 分支就丢掉这个 chip。
    #[test]
    fn real_percent_chip_appended_on_idle_too() {
        assert_eq!(render_tmux(&[], 0, Some(9)), "tfa:idle 9%");
    }

    /// 真实配额任务（2026-07-14）：status_bar_percent 开启时，daemon 侧把 Claude
    /// 的真实 5h 百分比传进来，tmux 状态行末尾追加一个 " {p}%" chip；不传（None）
    /// 时行为必须和今天完全一样——不出现任何 '%' 字符。
    #[test]
    fn real_percent_chip_appended_when_present() {
        let sessions = vec![sess("%1", Some("api"), SessionState::Working, 0)];
        assert!(render_tmux(&sessions, 0, Some(62)).ends_with(" 62%"));
        assert!(!render_tmux(&sessions, 0, None).contains('%'));
    }
}
