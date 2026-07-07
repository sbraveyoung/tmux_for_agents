use assert_cmd::Command;
use std::time::{Duration, Instant};

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
    assert!(started.elapsed() < Duration::from_millis(500), "hook too slow");
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
        .env("TMUX_PANE", "%8")
        .args(["hook", "claude", "user-prompt-submit"])
        .write_stdin(r#"{"prompt":"hello"}"#);
    cmd.assert().success();

    // 等 daemon 就绪后查询
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut status = Command::cargo_bin("tfa").unwrap();
    status.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .args(["list"]);
    let out = status.assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""pane_id":"%8""#), "got: {stdout}");

    // 清理：杀掉测试拉起的 daemon。
    // 注意：`pkill -f <sock>` 匹配不到 —— socket 路径只存在于子进程的环境变量
    // （TFA_SOCKET），不在 argv 里，daemon 的 argv 只有 "tfa daemon"。改用 lsof
    // 按打开的 socket 文件精确定位持有者 PID 再 kill，避免留下孤儿 daemon。
    if let Ok(out) = std::process::Command::new("lsof")
        .args(["-t", &sock.to_string_lossy()])
        .output()
    {
        for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
    }
}
