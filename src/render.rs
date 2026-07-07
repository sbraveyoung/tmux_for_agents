use crate::state::{AgentSession, SessionState};

pub fn render_tmux(sessions: &[AgentSession], now_ms: u64) -> String {
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
    if parts.is_empty() { return "tfa:idle".into(); }

    if let Some(w) = oldest_wait {
        let mins = now_ms.saturating_sub(w.state_since_ms) / 60_000;
        let name = w.session_name.as_deref().unwrap_or(&w.pane_id);
        parts.push(format!("⏸{} {mins}m", truncate_name(name)));
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
        }
    }

    #[test]
    fn empty_renders_idle() {
        assert_eq!(render_tmux(&[], 0), "tfa:idle");
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
        let line = render_tmux(&sessions, 300_000);
        assert_eq!(line, "⚡1 ⏸2 ✓1 ⏸web 4m");
    }

    #[test]
    fn dead_and_zero_counts_omitted() {
        let sessions = vec![sess("%1", None, SessionState::Dead, 0)];
        assert_eq!(render_tmux(&sessions, 0), "tfa:idle");
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
        assert_eq!(render_tmux(&sessions, 60_000), "⏸1 ⏸subscription_wor… 1m");
    }

    #[test]
    fn short_session_name_is_untouched() {
        let sessions = vec![sess(
            "%1",
            Some("api"),
            SessionState::WaitingInput { reason: "perm".into() },
            0,
        )];
        assert_eq!(render_tmux(&sessions, 0), "⏸1 ⏸api 0m");
    }
}
