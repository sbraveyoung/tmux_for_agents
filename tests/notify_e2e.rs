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

/// 兜底清理：Drop 时 kill 隔离 tmux server。镜像 tests/scanner_e2e.rs::TmuxKillOnDrop。
struct TmuxKillOnDrop(String);
impl Drop for TmuxKillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-f", "/dev/null", "-L", &self.0, "kill-server"]).output();
    }
}

/// spec §8：tmux 通道对无 attached client 的 pane 发 display-message 是非致命的
/// （静默吞，不崩 daemon）。用真实隔离 tmux server 而非 TFA_NO_NOTIFY sink——sink
/// 短路了真实 dispatch 路径，测不出 tmux_send() 本身对 no-clients 的处理。
/// `tmux new-session -d` 从不附着 client，天然满足 no-clients 前提，不用额外
/// 断开步骤。macOS 通道显式关掉，避免在跑测试的开发机上弹出真通知。
#[test]
fn notify_to_pane_with_no_attached_client_is_non_fatal() {
    let tmux_sock = format!("tfa-notify-e2e-noclient-{}", std::process::id());
    let new_session = Command::new("tmux")
        .args(["-f", "/dev/null", "-L", &tmux_sock, "new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .output()
        .expect("failed to start isolated tmux server");
    assert!(new_session.status.success(), "tmux new-session failed: {}", String::from_utf8_lossy(&new_session.stderr));
    let _tmux_guard = TmuxKillOnDrop(tmux_sock.clone());

    let pane_out = Command::new("tmux")
        .args(["-L", &tmux_sock, "list-panes", "-t", "main", "-F", "#{pane_id}"])
        .output()
        .expect("tmux list-panes failed to run");
    let pane = String::from_utf8(pane_out.stdout).unwrap().trim().to_string();
    assert!(!pane.is_empty(), "no pane id from isolated tmux session");

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    let state_dir = dir.path().to_path_buf();

    // tmux 通道开，macos 通道关（不在跑测试的开发机上弹真通知）；boot_grace 关，
    // 隔离出「净边沿→dispatch」这条链路本身。
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[notify.discipline]\nboot_grace_secs = 0\n[notify.channels.macos]\nenabled = false\n[notify.channels.tmux]\nenabled = true\n",
    )
    .unwrap();

    let stderr_path = dir.path().join("daemon.stderr");
    let daemon = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", &state_dir)
        .env("TFA_CONFIG_PATH", &config_path)
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .env("TFA_NO_SCAN", "1")
        .env("TFA_TMUX_SOCKET", &tmux_sock)
        .env_remove("TFA_NO_NOTIFY") // 走真实 dispatch 路径，不是 sink 短路
        .arg("daemon")
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn daemon");
    let mut guard = DaemonGuard(daemon);
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(sock.exists(), "daemon never created its socket");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
    cmd.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", &state_dir)
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", &pane)
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

    // 给异步 notifier 线程时间跑完对无 client pane 的 tmux display-message
    // （channels.rs::LOCAL_CHANNEL_CAP 硬超时上限 5s，正常情况远快于此）。
    std::thread::sleep(Duration::from_millis(500));

    // 核心断言：daemon 进程本身没有因为这次 dispatch 退出/崩溃。
    match guard.0.try_wait() {
        Ok(None) => {} // 仍在跑，符合预期
        Ok(Some(status)) => panic!("daemon exited after no-clients dispatch: {status}"),
        Err(e) => panic!("failed to poll daemon status: {e}"),
    }

    // daemon 还在正常服务（no-clients dispatch 没有把它拖死/挂住）。
    let mut list_cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
    list_cmd.env("TFA_SOCKET", &sock).env("TFA_STATE_DIR", &state_dir).env("TFA_NO_SPAWN", "1").arg("list");
    let out = list_cmd.output().expect("list command failed to run");
    assert!(out.status.success(), "daemon stopped serving after no-clients dispatch");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&pane), "hooked pane missing from post-dispatch list: {stdout}");

    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(!stderr.to_lowercase().contains("panic"), "daemon stderr shows a panic: {stderr}");
}
