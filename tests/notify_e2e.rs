//! e2e：起 daemon（TFA_NO_NOTIFY=1），模拟 hook 序列，断言 notifier sink 收到恰当净边沿。
//!
//! 不需要真实 tmux 会话：hook 路径只要求 `TMUX_PANE` 环境变量存在（不要求 pane 真的
//! 存在于某个 tmux server 里），`resolve_session_name` 指向一个不存在的隔离 tmux socket
//! 会快速失败返回 None（镜像 tests/hook_cmd.rs::hook_autospawns_daemon_and_event_lands 的
//! `tfa-test-none-<pid>` 手法）。`TFA_NO_SCAN=1` 关掉 scanner，隔离出 hook 路径本身产生
//! 的净边沿，不被扫描轮次干扰。

use std::io::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

/// 兜底清理：Drop 时 kill+wait 直接持有的 daemon 子进程，断言 panic 也不会留下孤儿。
struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {timeout_ms}ms");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sink_lines(state_dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(state_dir.join("notify-sink.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad sink json: {e}; line: {l}")))
        .collect()
}

#[test]
fn waiting_input_hook_produces_one_notification_and_does_not_block() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    let state_dir = dir.path().to_path_buf();

    // 默认 config 的 boot_grace_secs=30s 会吞掉刚启动就来的第一条边沿——测试专用
    // config 把它关掉，隔离出「净边沿→通知」这条链路本身，不受 boot grace 干扰。
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[notify.discipline]\nboot_grace_secs = 0\n").unwrap();

    let daemon = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", &state_dir)
        .env("TFA_CONFIG_PATH", &config_path)
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .env("TFA_NO_SCAN", "1")
        .env("TFA_NO_NOTIFY", "1")
        // 指向一个不存在的隔离 tmux server：respond() 的 session-name 回填会快速
        // 失败返回 None，既不打到开发者真实 tmux，也不拖慢 hook 往返。
        .env("TFA_TMUX_SOCKET", format!("tfa-notify-e2e-none-{}", std::process::id()))
        .arg("daemon")
        .spawn()
        .expect("spawn daemon");
    let _guard = DaemonGuard(daemon);
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(sock.exists(), "daemon never created its socket");

    let pane = "%42";
    let started = Instant::now();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
    cmd.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", &state_dir)
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", pane)
        .args(["hook", "claude", "notification"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"message":"needs your permission"}"#)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "tfa hook exited non-zero");
    // 护栏：edges() 计算 + tx.send 必须留在 respond() 锁内/出锁的非阻塞路径上——
    // 真正的通知 IO 归 notifier 线程异步消费，hook 往返绝不等它。
    assert!(started.elapsed() < Duration::from_millis(2000), "hook too slow (blocked on notify IO?)");

    wait_until(|| !sink_lines(&state_dir).is_empty(), 3000);
    let lines = sink_lines(&state_dir);
    assert_eq!(lines.len(), 1, "净边沿只应发一次通知; sink: {lines:?}");
    assert_eq!(lines[0]["kind"], "waiting_input");
    assert_eq!(lines[0]["pane"], pane);
}
