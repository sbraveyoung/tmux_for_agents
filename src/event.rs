use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Custom(String),
}

impl AgentKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            other => Self::Custom(other.to_string()),
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Custom(n) => n,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventKind {
    SessionStart,
    UserPromptSubmit,
    Notification,
    Stop,
    SessionEnd,
    Activity,
}

impl EventKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "session-start" => Self::SessionStart,
            "user-prompt-submit" => Self::UserPromptSubmit,
            "notification" => Self::Notification,
            "stop" => Self::Stop,
            "session-end" => Self::SessionEnd,
            "activity" | "post-tool-use" => Self::Activity,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub agent: AgentKind,
    pub pane_id: String,
    pub kind: EventKind,
    pub reason: Option<String>,
    pub prompt: Option<String>,
    pub cwd: Option<String>,
    pub at_ms: u64,
}

fn str_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key)?.as_str().map(|s| s.to_string())
}

impl AgentEvent {
    pub fn from_hook(agent: &str, event: &str, pane: &str, payload: &Value, at_ms: u64) -> Option<Self> {
        let kind = EventKind::parse(event)?;
        Some(Self {
            agent: AgentKind::parse(agent),
            pane_id: pane.to_string(),
            kind,
            reason: str_field(payload, "message"),
            prompt: str_field(payload, "prompt").map(|p| p.chars().take(120).collect()),
            cwd: str_field(payload, "cwd"),
            at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_notification_maps_to_waiting_reason() {
        let payload = json!({
            "session_id": "abc", "cwd": "/tmp/p",
            "message": "Claude needs your permission to use Bash"
        });
        let ev = AgentEvent::from_hook("claude", "notification", "%7", &payload, 1000).unwrap();
        assert!(matches!(ev.kind, EventKind::Notification));
        assert_eq!(ev.reason.as_deref(), Some("Claude needs your permission to use Bash"));
        assert_eq!(ev.pane_id, "%7");
        assert_eq!(ev.cwd.as_deref(), Some("/tmp/p"));
    }

    #[test]
    fn prompt_is_truncated_to_120_chars() {
        let long = "x".repeat(300);
        let payload = json!({ "prompt": long });
        let ev = AgentEvent::from_hook("claude", "user-prompt-submit", "%1", &payload, 0).unwrap();
        assert_eq!(ev.prompt.as_ref().unwrap().chars().count(), 120);
    }

    #[test]
    fn unknown_event_returns_none_and_unknown_agent_is_custom() {
        assert!(AgentEvent::from_hook("claude", "no-such-event", "%1", &serde_json::Value::Null, 0).is_none());
        let ev = AgentEvent::from_hook("hermes", "stop", "%1", &serde_json::Value::Null, 0).unwrap();
        assert!(matches!(ev.agent, AgentKind::Custom(ref n) if n == "hermes"));
    }
}
