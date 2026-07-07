use std::process::Command;
use std::time::Duration;

/// 兜底清理：Drop 时杀掉隔离 tmux server。即使断言 panic 也不会留下孤儿 server ——
/// 镜像 tests/hook_cmd.rs::KillOnDrop 的思路，但清理对象是 tmux 而非 daemon 进程。
/// `-f /dev/null` 不加载开发者的 ~/.tmux.conf（会拉起 TPM / marmonitor /
/// tmux-agent-sidebar 等插件，拖慢且不确定）；kill-server 时该参数无实际效果
/// （server 早已用 new-session 时的 -f 启动过），但保留以防将来变成首次调用。
struct TmuxKillOnDrop(String);

impl Drop for TmuxKillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-f", "/dev/null", "-L", &self.0, "kill-server"])
            .output();
    }
}

/// 兜底清理：Drop 时杀掉自动拉起的 daemon。daemon 由被测二进制间接拉起（拿不到
/// Child 句柄），故用 lsof 按打开的 socket 文件定位持有者 PID。
/// 注意：`pkill -f <sock>` 匹配不到 —— socket 路径只存在于子进程的环境变量
/// （TFA_SOCKET），不在 argv 里，daemon 的 argv 只有 "tfa daemon"（T6 已验证）。
struct KillOnDrop(Vec<u32>);

impl KillOnDrop {
    fn for_socket(sock: &std::path::Path) -> Self {
        let pids = Command::new("lsof")
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
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

/// 完整链路：隔离 tmux server 里跑假 agent（用 tfa hook 模拟 claude hooks），
/// 断言 tfa status 输出。tmux 不存在时跳过。
#[test]
fn e2e_fake_agent_lifecycle_reflected_in_status() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skip: tmux not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tmux_sock = format!("tfa-e2e-{}", std::process::id());
    let bin = env!("CARGO_BIN_EXE_tfa");
    let sock_path = dir.path().join("tfa.sock");
    let envs = [
        ("TFA_SOCKET", sock_path.to_string_lossy().into_owned()),
        ("TFA_STATE_DIR", dir.path().to_string_lossy().into_owned()),
        ("TFA_TMUX_SOCKET", tmux_sock.clone()),
    ];

    // `-f /dev/null`: 不加载开发者的 ~/.tmux.conf，避免把 TPM / marmonitor /
    // tmux-agent-sidebar 等插件拉进测试 server —— 慢且不确定。
    let tmux = |args: &[&str]| {
        let mut c = Command::new("tmux");
        c.args(["-f", "/dev/null", "-L", &tmux_sock]).args(args);
        for (k, v) in &envs { c.env(k, v); }
        c.output().unwrap()
    };

    // 起隔离 tmux + 一个 pane；pane 里模拟 agent 生命周期
    assert!(tmux(&["new-session", "-d", "-s", "proj", "-x", "80", "-y", "24"]).status.success());
    // 断言之前登记清理：new-session 成功后，任何后续 panic 都不会留下孤儿 tmux server。
    let tmux_guard = TmuxKillOnDrop(tmux_sock.clone());

    let script = format!(
        "echo '{{\"prompt\":\"build it\"}}' | {bin} hook claude user-prompt-submit; \
         echo '{{\"message\":\"needs permission\"}}' | {bin} hook claude notification"
    );
    assert!(tmux(&["send-keys", "-t", "proj:0.0", &script, "Enter"]).status.success());

    // 等 daemon 就绪（由第一个 hook 自动拉起）后登记 daemon 清理守卫。
    for _ in 0..100 {
        if sock_path.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    let daemon_guard = KillOnDrop::for_socket(&sock_path);

    // 轮询 status 直到状态就位（daemon 由 hook 自动拉起）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let expected_frag = "⏸proj";
    loop {
        let mut c = Command::new(bin);
        for (k, v) in &envs { c.env(k, v); }
        c.env("TFA_NO_SPAWN", "1").args(["status", "--format", "tmux"]);
        let out = String::from_utf8(c.output().unwrap().stdout).unwrap();
        if out.contains(expected_frag) { break; }
        assert!(
            std::time::Instant::now() < deadline,
            "status never showed waiting agent; last output: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // 显式清理（Drop guard 是兜底；这里正常路径下主动收尾更快）。
    drop(daemon_guard);
    drop(tmux_guard);
}
