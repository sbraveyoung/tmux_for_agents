use super::NotifyEvent;
use crate::config::NotifyConfig;
use serde_json::json;
use std::time::{Duration, Instant};

/// 子进程硬超时：spawn 后轮询 try_wait，超时即 kill。用于 macOS/tmux 通道，
/// 防挂住的 osascript/tmux 堵死串行 notifier 队列（C1 对所有通道 IO 的硬超时要求）。
const LOCAL_CHANNEL_CAP: Duration = Duration::from_secs(5);
fn run_capped(mut cmd: std::process::Command) {
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).stdin(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if start.elapsed() >= LOCAL_CHANNEL_CAP { let _ = child.kill(); return; }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
thread_local! {
    static SINK: std::cell::RefCell<Vec<NotifyEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
pub fn test_sink_clear() { SINK.with(|s| s.borrow_mut().clear()); }
#[cfg(test)]
pub fn test_sink_take() -> Vec<NotifyEvent> { SINK.with(|s| std::mem::take(&mut *s.borrow_mut())) }

fn no_notify() -> bool { std::env::var("TFA_NO_NOTIFY").as_deref() == Ok("1") }

/// 派发到所有 enabled 通道。失败静默吞（不重试/不告警）。
pub fn dispatch(ev: &NotifyEvent, cfg: &NotifyConfig) {
    if no_notify() {
        // 真二进制也可观测：append 到 state_dir/notify-sink.jsonl（e2e 子进程断言用；
        // 不能只用 #[cfg(test)] thread_local——那不编进 cargo_bin("tfa") 起的真进程）。
        let line = json!({"kind": ev.kind.as_str(), "pane": ev.pane_id, "session": ev.session_name, "title": ev.title}).to_string();
        let _ = std::fs::create_dir_all(crate::paths::state_dir());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
            .open(crate::paths::state_dir().join("notify-sink.jsonl")) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        #[cfg(test)]
        SINK.with(|s| s.borrow_mut().push(ev.clone())); // 进程内单测用
        return;
    }
    if cfg.channels.macos.enabled { macos_send(ev); }
    if cfg.channels.tmux.enabled { tmux_send(ev); }
    if cfg.channels.http.enabled && !cfg.channels.http.url.is_empty() { http_send(ev, &cfg.channels.http); }
}

fn macos_send(ev: &NotifyEvent) {
    // terminal-notifier 检测到就用，否则 osascript 兜底。失败静默；子进程带硬超时。
    let has_tn = std::process::Command::new("sh").arg("-c").arg("command -v terminal-notifier")
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()
        .map(|s| s.success()).unwrap_or(false);
    if has_tn {
        let mut c = std::process::Command::new("terminal-notifier");
        c.args(["-title", &ev.title, "-message", &ev.body]);
        run_capped(c);
    } else {
        let script = format!("display notification {:?} with title {:?}", ev.body, ev.title);
        let mut c = std::process::Command::new("osascript");
        c.args(["-e", &script]);
        run_capped(c);
    }
}

fn tmux_send(ev: &NotifyEvent) {
    // display-message 到目标 pane；无 attached client 返回 no clients，静默吞；带硬超时。
    let mut c = std::process::Command::new("tmux");
    c.args(crate::paths::tmux_args());
    c.args(["display-message", "-t", &ev.pane_id, &format!("[tfa] {}", ev.title)]);
    run_capped(c);
}

/// bark: url 末段是 device_key；ntfy: url 末段是 topic；generic: 原样 POST url。
pub fn http_payload(format: &str, ev: &NotifyEvent, key_or_topic: &str) -> serde_json::Value {
    let session = ev.session_name.clone().unwrap_or_else(|| ev.pane_id.clone());
    match format {
        "bark" => json!({ "device_key": key_or_topic, "title": ev.title, "body": ev.body, "group": "tfa" }),
        "ntfy" => json!({ "topic": key_or_topic, "title": ev.title, "message": ev.body, "tags": ["robot"] }),
        _ => json!({ "kind": ev.kind.as_str(), "session": session, "pane": ev.pane_id, "title": ev.title, "body": ev.body }),
    }
}

fn last_segment(url: &str) -> String {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn http_send(ev: &NotifyEvent, http: &crate::config::HttpChannel) {
    let key_or_topic = last_segment(&http.url);
    let payload = http_payload(&http.format, ev, &key_or_topic);
    // bark 固定 POST {base}/push；ntfy/generic POST 到 url 根。
    let target = if http.format == "bark" {
        let base = http.url.trim_end_matches('/');
        let base = base.strip_suffix(&format!("/{key_or_topic}")).unwrap_or(base);
        format!("{base}/push")
    } else {
        http.url.clone()
    };
    let timeout = Duration::from_millis(http.timeout_ms.clamp(200, 10_000));
    let mut req = ureq::post(&target).config().timeout_global(Some(timeout)).build();
    for (k, v) in &http.headers { req = req.header(k.as_str(), v.as_str()); }
    let _ = req.send_json(payload); // 失败静默吞
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NotifyEvent, NotifyKind};

    fn ev() -> NotifyEvent {
        NotifyEvent { session_key: "sess-1".into(), pane_id: "%3".into(), session_name: Some("api".into()),
            kind: NotifyKind::WaitingInput, title: "api 等待输入".into(), body: "needs permission".into() }
    }

    #[test]
    fn bark_payload_has_required_fields() {
        let v = http_payload("bark", &ev(), "devkey123");
        assert_eq!(v["device_key"], "devkey123");
        assert_eq!(v["title"], "api 等待输入");
        assert_eq!(v["body"], "needs permission");
    }

    #[test]
    fn ntfy_payload_uses_topic_and_message() {
        let v = http_payload("ntfy", &ev(), "tfa-alerts");
        assert_eq!(v["topic"], "tfa-alerts");
        assert_eq!(v["title"], "api 等待输入");
        assert_eq!(v["message"], "needs permission");
    }

    #[test]
    fn generic_payload_carries_kind_and_session() {
        let v = http_payload("generic-json", &ev(), "");
        assert_eq!(v["kind"], "waiting_input");
        assert_eq!(v["session"], "api");
        assert_eq!(v["title"], "api 等待输入");
    }

    #[test]
    fn no_notify_env_routes_to_sink_not_real_dispatch() {
        // isolate the notify-sink.jsonl write to a tempdir so the test never
        // touches a real ~/.local/state/tfa/ (a live daemon's state dir).
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TFA_STATE_DIR", dir.path());
        std::env::set_var("TFA_NO_NOTIFY", "1");
        test_sink_clear();
        let cfg = crate::config::NotifyConfig::default(); // macos+tmux on, http off
        dispatch(&ev(), &cfg);
        std::env::remove_var("TFA_NO_NOTIFY");
        std::env::remove_var("TFA_STATE_DIR");
        let sunk = test_sink_take();
        assert_eq!(sunk.len(), 1, "TFA_NO_NOTIFY 下事件进 sink，不真发");
        assert_eq!(sunk[0].kind.as_str(), "waiting_input");
    }
}
