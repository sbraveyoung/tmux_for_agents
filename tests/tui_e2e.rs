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
    // TFA_CONFIG_PATH 指向不存在的路径——隔离测试机上真实 ~/.config/tfa/config.toml
    // 的 [tui] 设置（同 notify_cmd.rs 的隔离方式），避免测试受运行机器本地配置影响。
    let shell_cmd = format!(
        "TFA_SOCKET='{}' TFA_STATE_DIR='{}' TFA_NO_SPAWN=1 TFA_CONFIG_PATH='{}/nonexistent-config.toml' '{}' tui; echo $? > '{}'",
        sock_path.display(),
        dir.path().display(),
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
    // Part1: 列表列头（真实子进程 + tmux capture-pane 全链路，不只是 TestBackend 单测）。
    assert!(last.contains("agent") && last.contains("摘要"), "column header row missing:\n{last}");
    // Part3: Footer 新鲜度后缀（真实子进程链路）——精确的 <2s/≥2s 阈值文案已有
    // fmt_freshness 单测钉死，这里只验证前缀+分隔符确实拼接了后缀，避免在偶尔
    // 稍慢的 CI 机器上因为具体是「刚刚」还是「2s前」而 flaky。
    assert!(last.contains("已连接·"), "freshness suffix missing:\n{last}");

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
