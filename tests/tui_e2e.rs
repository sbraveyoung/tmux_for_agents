//! tui 全链路 e2e：隔离 tmux server 的 pane 里跑 `tfa tui`（绝不裸 spawn——
//! 它会打开 /dev/tty 抢占测试终端），断言真实渲染 + q 退出码 0。
//! 守卫结构镜像 tests/scanner_e2e.rs（TmuxKillOnDrop / DaemonGuard）。

use std::io::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

struct TmuxKillOnDrop(String);
impl Drop for TmuxKillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-f", "/dev/null", "-L", &self.0, "kill-server"]).output();
    }
}

struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmux(sock: &str, args: &[&str]) -> std::process::Output {
    Command::new("tmux")
        .args(["-f", "/dev/null", "-L", sock])
        .args(args)
        .output()
        .expect("tmux command failed to run")
}

#[test]
fn tui_renders_sessions_and_quits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tfa.sock");
    let tmux_sock = format!("tfatui-{}", std::process::id());
    let _tmux_guard = TmuxKillOnDrop(tmux_sock.clone());
    let bin = env!("CARGO_BIN_EXE_tfa");

    // 1. 隔离 daemon（禁 scanner/通知，跳过 tmux 存活检查）
    let daemon = Command::new(bin)
        .env("TFA_SOCKET", &sock_path)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_TMUX_SOCKET", &tmux_sock)
        .env("TFA_NO_SCAN", "1")
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .env("TFA_NO_NOTIFY", "1")
        .arg("daemon")
        .spawn()
        .unwrap();
    let _daemon_guard = DaemonGuard(daemon);
    let start = Instant::now();
    while !sock_path.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "daemon socket never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 2. 注入一个 waiting_input 会话（hook 从 TMUX_PANE env + stdin payload 取数）
    let mut hook = Command::new(bin)
        .env("TFA_SOCKET", &sock_path)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", "%7")
        .args(["hook", "claude", "notification"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(br#"{"message":"needs permission"}"#).unwrap();
    assert!(hook.wait().unwrap().success());

    // 3. 隔离 tmux 的 pane 里起 tui（120x30，宽布局）
    let exit_marker = dir.path().join("tui-exit");
    let shell_cmd = format!(
        "TFA_SOCKET='{}' TFA_STATE_DIR='{}' TFA_NO_SPAWN=1 '{}' tui; echo $? > '{}'",
        sock_path.display(),
        dir.path().display(),
        bin,
        exit_marker.display()
    );
    let out = tmux(&tmux_sock, &["new-session", "-d", "-x", "120", "-y", "30", &shell_cmd]);
    assert!(out.status.success(), "tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr));

    // 4. 轮询 capture-pane 直到渲染出注入的会话
    let start = Instant::now();
    let mut last;
    loop {
        let cap = tmux(&tmux_sock, &["capture-pane", "-p", "-t", "0"]);
        last = String::from_utf8_lossy(&cap.stdout).into_owned();
        if last.contains("⏸1") && last.contains("%7") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "tui never rendered injected session; last capture:\n{last}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(last.contains("等 "), "waiting summary missing:\n{last}");
    assert!(last.contains("needs permission"), "reason missing:\n{last}");
    assert!(last.contains("已连接"), "footer conn state missing:\n{last}");

    // 5. q 退出 → 退出码 0（终端生命周期干净的最低自动化证明）
    tmux(&tmux_sock, &["send-keys", "-t", "0", "q"]);
    let start = Instant::now();
    while !exit_marker.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "tui did not exit after q");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read_to_string(&exit_marker).unwrap().trim(),
        "0",
        "tui exit code after q"
    );
}
