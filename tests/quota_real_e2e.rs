//! 真实配额 e2e：默认关 = 零行为变化；notify-sink 无 quota_alert 事件。
//! （real=true 的正向链路依赖外网+凭证，属人工真机验收——spec §11。）
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn default_config_spawns_no_quota_fetcher_and_no_alerts() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    let daemon = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_CONFIG_PATH", dir.path().join("nonexistent.toml"))
        .env("TFA_NO_SCAN", "1").env("TFA_SKIP_TMUX_CHECK", "1").env("TFA_NO_NOTIFY", "1")
        .arg("daemon").spawn().unwrap();
    struct Guard(std::process::Child);
    impl Drop for Guard { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let _g = Guard(daemon);
    let start = Instant::now();
    while !sock.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
    }
    // 注入一个会话（触发 tick/快照管线），再取快照确认 quota 段仍是 local_estimate 语义
    let mut hook = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock).env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1").env("TMUX_PANE", "%1")
        .args(["hook", "claude", "user-prompt-submit"])
        .stdin(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write as _;
    hook.stdin.take().unwrap().write_all(b"{}").unwrap();
    assert!(hook.wait().unwrap().success());
    let out = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock).env("TFA_STATE_DIR", dir.path()).env("TFA_NO_SPAWN", "1")
        .arg("list").output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for q in json["quota"].as_array().unwrap() {
        assert_ne!(q["source"], "real_api", "默认关时绝不能出现 real_api");
    }
    // sink 里不得有 quota_alert
    let sink = std::fs::read_to_string(dir.path().join("notify-sink.jsonl")).unwrap_or_default();
    assert!(!sink.contains("quota_alert"), "sink: {sink}");
}
