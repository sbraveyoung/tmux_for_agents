use std::process::Command;
use std::time::{Duration, Instant};

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return;
        }
        assert!(Instant::now() < deadline, "condition not met within {timeout_ms}ms");
        std::thread::sleep(Duration::from_millis(50));
    }
}

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

#[test]
fn socket_dir_perms_are_0700_and_stale_activity_markers_pruned() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    // socket 放在一个全新的子目录里，把它的权限和 tempdir 根目录（tempfile crate
    // 默认就是 0700，无法证明是我们的代码起作用）区分开。
    let sock = dir.path().join("run/tfa.sock");

    let mut c = Command::new(env!("CARGO_BIN_EXE_tfa"));
    c.env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_SKIP_TMUX_CHECK", "1")
        .env("TFA_NO_SCAN", "1")
        .env("TFA_TMUX_CHECK_INTERVAL_MS", "200")
        .arg("daemon");
    let mut child = c.spawn().unwrap();

    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(sock.exists(), "daemon never created its socket");

    // 1. socket 父目录权限必须是 0700 且属主是当前用户创建的（set_permissions 生效）。
    let sock_dir = sock.parent().unwrap();
    let mode = std::fs::metadata(sock_dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "socket dir perms were {:o}", mode & 0o777);

    // 2. 预置一个 mtime 拨旧 2h 的 activity marker（超过 MARKER_MAX_AGE=1h）和一个
    //    新鲜的 marker；跑过至少一个维护周期（TFA_TMUX_CHECK_INTERVAL_MS=200）后，
    //    旧的应被清理、新的应保留。
    let old_marker = dir.path().join("activity-old");
    let new_marker = dir.path().join("activity-new");
    std::fs::write(&old_marker, "").unwrap();
    std::fs::write(&new_marker, "").unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&old_marker)
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    wait_until(|| !old_marker.exists() && new_marker.exists(), 3000);

    child.kill().unwrap();
    child.wait().unwrap();
}
