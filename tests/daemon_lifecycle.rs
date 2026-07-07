use std::process::Command;
use std::time::Duration;

fn daemon_cmd(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tfa"));
    c.env("TFA_SOCKET", dir.join("tfa.sock"))
        .env("TFA_STATE_DIR", dir)
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .arg("daemon");
    c
}

#[test]
fn second_daemon_exits_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = daemon_cmd(dir.path()).spawn().unwrap();
    let sock = dir.path().join("tfa.sock");
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    // 第二个实例应拿不到锁，静默退出 0
    let status = daemon_cmd(dir.path()).status().unwrap();
    assert!(status.success());
    // 第一个还活着
    assert!(first.try_wait().unwrap().is_none());
    first.kill().unwrap();
    first.wait().unwrap();
}

#[test]
fn daemon_exits_when_tmux_dead() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_tfa"));
    c.env("TFA_SOCKET", dir.path().join("tfa.sock"))
        .env("TFA_STATE_DIR", dir.path())
        // 指向必然不存在的 tmux server；不设 SKIP
        .env("TFA_TMUX_SOCKET", "tfa-test-definitely-absent")
        .env("TFA_TMUX_CHECK_INTERVAL_MS", "100") // 测试加速
        .arg("daemon");
    let mut child = c.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon didn't exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn snapshot_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    // 直接预置快照文件，daemon 启动应加载
    std::fs::write(
        dir.path().join("snapshot.json"),
        r#"{"sessions":{"%5":{"pane_id":"%5","agent":"claude","session_name":"proj",
            "state":"done","state_since_ms":1,"current_task":null,"cwd":null,"last_activity_ms":1}}}"#,
    ).unwrap();
    let mut child = daemon_cmd(dir.path()).spawn().unwrap();
    let sock = dir.path().join("tfa.sock");
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    s.write_all(b"{\"op\":\"snapshot\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    assert!(line.contains(r#""pane_id":"%5""#), "got: {line}");
    child.kill().unwrap();
    child.wait().unwrap();
}
