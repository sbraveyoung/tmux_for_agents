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

/// 兜底清理：Drop 时 kill + wait 直接持有的 daemon 子进程（镜像
/// tests/scanner_e2e.rs::DaemonGuard）——与下面的 KillOnDrop 不同，这里测试
/// 自己 spawn 了 daemon、握有 Child 句柄，可以直接收尸，不留僵尸进程。
struct DaemonChildGuard(std::process::Child);

impl Drop for DaemonChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

/// M1→M2 快照升级契约钉子：M1 时代写盘的 snapshot.json（没有 source/model/
/// context/tokens 等 M2 新增字段）必须能被 M2 daemon 原样加载，并在 `tfa list`
/// 中以 serde default 补齐后的形状对外服务——source 缺省补成 "hook"，model 等
/// 富化字段保持 null，daemon 全程不 crash。不依赖 tmux（TFA_SKIP_TMUX_CHECK=1）
/// 也不受 scanner 纠偏干扰（TFA_NO_SCAN=1）。
#[test]
fn daemon_loads_m1_era_snapshot_and_serves_enriched_shape() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_tfa");
    let sock_path = dir.path().join("tfa.sock");
    let envs = [
        ("TFA_SOCKET", sock_path.to_string_lossy().into_owned()),
        ("TFA_STATE_DIR", dir.path().to_string_lossy().into_owned()),
    ];

    // 1. 预写 M1 形状的 snapshot.json——关键点：条目里没有任何 M2 新增字段。
    let m1_snapshot = r#"{"sessions":{"%1":{"pane_id":"%1","agent":"claude","session_name":"api","state":"working","state_since_ms":100,"current_task":"fix","cwd":"/tmp/p","last_activity_ms":200}}}"#;
    std::fs::write(dir.path().join("snapshot.json"), m1_snapshot).unwrap();

    // 2. 直接起 daemon（不走 hook 自动拉起），并在 socket 就绪前登记清理守卫。
    let mut cmd = Command::new(bin);
    for (k, v) in &envs { cmd.env(k, v); }
    cmd.env("TFA_SKIP_TMUX_CHECK", "1").env("TFA_NO_SCAN", "1").arg("daemon");
    let mut daemon = DaemonChildGuard(cmd.spawn().expect("spawn daemon"));
    for _ in 0..200 {
        if sock_path.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(sock_path.exists(), "daemon never created its socket");

    // 3. tfa list：恰好一条 %1，source 被 default 补成 "hook"，model 保持 null。
    let mut c = Command::new(bin);
    for (k, v) in &envs { c.env(k, v); }
    c.env("TFA_NO_SPAWN", "1").arg("list");
    let out = c.output().expect("run tfa list");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let sessions: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("bad list json: {e}; got: {stdout}"));
    let arr = sessions.as_array().expect("list output is a JSON array");
    assert_eq!(arr.len(), 1, "expected exactly one session, got: {stdout}");
    assert_eq!(arr[0]["pane_id"], "%1");
    assert_eq!(arr[0]["source"], "hook", "M1 entry must default to source=hook: {stdout}");
    assert!(arr[0]["model"].is_null(), "M1 entry has no model; must stay null: {stdout}");

    // 4. daemon 没有 crash/panic：list 之后进程仍在运行。
    assert!(
        daemon.0.try_wait().expect("try_wait daemon").is_none(),
        "daemon exited unexpectedly after serving the upgraded snapshot"
    );
}
