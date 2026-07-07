use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::Duration;

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) { let _ = self.0.kill(); }
}

fn start_daemon(dir: &std::path::Path) -> (DaemonGuard, std::path::PathBuf) {
    let sock = dir.join("tfa.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir)
        .env("TFA_SKIP_TMUX_CHECK", "1") // 测试环境无 tmux
        .arg("daemon")
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if sock.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    (DaemonGuard(child), sock)
}

fn roundtrip(sock: &std::path::Path, line: &str) -> String {
    let mut s = UnixStream::connect(sock).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    let mut reader = BufReader::new(s);
    let mut resp = String::new();
    reader.read_line(&mut resp).unwrap();
    resp
}

#[test]
fn hook_then_snapshot_reflects_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_guard, sock) = start_daemon(dir.path());

    let ok = roundtrip(&sock,
        r#"{"op":"hook","agent":"claude","event":"user-prompt-submit","pane":"%9","payload":{"prompt":"hi"}}"#);
    assert!(ok.contains(r#""result":"ok""#), "got: {ok}");

    let snap = roundtrip(&sock, r#"{"op":"snapshot"}"#);
    assert!(snap.contains(r#""pane_id":"%9""#), "got: {snap}");
    assert!(snap.contains(r#""state":"working""#), "got: {snap}");
}

#[test]
fn malformed_request_returns_error_not_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (_guard, sock) = start_daemon(dir.path());
    let resp = roundtrip(&sock, "not json at all");
    assert!(resp.contains(r#""result":"error""#));
    // daemon 仍活着
    let snap = roundtrip(&sock, r#"{"op":"snapshot"}"#);
    assert!(snap.contains("snapshot"));
}
