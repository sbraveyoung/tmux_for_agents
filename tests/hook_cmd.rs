use assert_cmd::Command;
use std::time::{Duration, Instant};

/// 兜底清理：drop 时 kill 记录的 PID —— 即使后续断言 panic 也不会留下孤儿 daemon。
/// 镜像 tests/daemon_socket.rs 的 DaemonGuard 模式；此处 daemon 由被测进程间接
/// 拉起（拿不到 Child 句柄），故用 lsof 按打开的 socket 文件定位持有者 PID。
/// 注意：`pkill -f <sock>` 匹配不到 —— socket 路径只存在于子进程的环境变量
/// （TFA_SOCKET），不在 argv 里，daemon 的 argv 只有 "tfa daemon"。
struct KillOnDrop(Vec<u32>);

impl KillOnDrop {
    fn for_socket(sock: &std::path::Path) -> Self {
        let pids = std::process::Command::new("lsof")
            .args(["-t", &sock.to_string_lossy()])
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .filter_map(|p| p.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        Self(pids)
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

#[test]
fn hook_without_daemon_exits_zero_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", "%3")
        .args(["hook", "claude", "stop"])
        .write_stdin("{}");
    cmd.assert().success();
    // generous envelope: guards against hangs, not scheduler jitter; 100ms IO discipline is enforced in client.rs
    assert!(started.elapsed() < Duration::from_millis(2000), "hook too slow");
}

#[test]
fn hook_without_tmux_pane_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env_remove("TMUX_PANE")
        .args(["hook", "claude", "stop"])
        .write_stdin("{}");
    cmd.assert().success();
}

#[test]
fn hook_autospawns_daemon_and_event_lands() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    // 无 daemon；hook 应自动拉起并送达
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_SKIP_TMUX_CHECK", "1")
        // autospawn 拉起的 daemon 继承本进程环境；指向不存在的 tmux server 让
        // Hook 分支的 session-name 解析确定性地失败（None），不打到真实 tmux。
        .env("TFA_TMUX_SOCKET", format!("tfa-test-none-{}", std::process::id()))
        .env("TMUX_PANE", "%8")
        .args(["hook", "claude", "user-prompt-submit"])
        .write_stdin(r#"{"prompt":"hello"}"#);
    cmd.assert().success();

    // 等 daemon 就绪后查询
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    // 断言之前就登记清理：断言 panic 也会在栈展开时 kill daemon，绝不留孤儿。
    let _guard = KillOnDrop::for_socket(&sock);

    let mut status = Command::cargo_bin("tfa").unwrap();
    status.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .args(["list"]);
    let out = status.assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""pane_id":"%8""#), "got: {stdout}");
}
