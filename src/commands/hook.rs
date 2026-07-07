use crate::client;
use crate::protocol::Request;
use std::io::Read;
use std::time::{Duration, SystemTime};

/// spec §6: PostToolUse is a pure activity heartbeat, client-side throttled —
/// skip sending if we already reported activity for this pane within this window.
const ACTIVITY_THROTTLE: Duration = Duration::from_secs(2);

/// hook 纪律：任何路径都 exit 0，绝不阻塞 agent。
pub fn run(agent: &str, event: &str) -> ! {
    let Ok(pane) = std::env::var("TMUX_PANE") else { std::process::exit(0) };
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let payload = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);

    if is_activity_event(event) && activity_recently_reported(&pane) {
        std::process::exit(0);
    }

    let _ = client::request(&Request::Hook {
        agent: agent.to_string(),
        event: event.to_string(),
        pane: Some(pane),
        payload,
    });
    std::process::exit(0)
}

fn is_activity_event(event: &str) -> bool {
    matches!(event, "post-tool-use" | "activity")
}

/// True (and skip) if this pane already reported activity less than
/// ACTIVITY_THROTTLE ago; otherwise best-effort marks "now" and returns false
/// so the caller proceeds to send.
///
/// Hook discipline is sacred: every filesystem op here is best-effort. Any
/// failure — missing state dir, permission error, unsupported mtime,
/// whatever — must silently fall through to sending the event, never crash
/// or block the hook.
fn activity_recently_reported(pane: &str) -> bool {
    let marker = crate::paths::state_dir().join(format!("activity-{}", sanitize_pane_id(pane)));

    let throttled = std::fs::metadata(&marker)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|elapsed| elapsed < ACTIVITY_THROTTLE);

    if !throttled {
        let _ = std::fs::create_dir_all(crate::paths::state_dir());
        let now_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // Write (not just create): content must change on every touch so
        // mtime actually advances even if the marker already existed.
        let _ = std::fs::write(&marker, now_ms.to_string());
    }

    throttled
}

fn sanitize_pane_id(pane: &str) -> String {
    pane.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}
