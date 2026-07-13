//! 真实配额 e2e：默认关 = 零行为变化——quota 段照常以 local_estimate 语义产出
//! （每个条目 source=="local_estimate" 且绝不编造百分比），notify-sink 无
//! quota_alert 事件。（real=true 的正向链路依赖外网+凭证，属人工真机验收——spec §11。）
//!
//! 需要真实 scanner tick：QuotaCache::refresh 只在 scanner tick 里被调用，
//! TFA_NO_SCAN=1 下 quota 段恒为空数组、逐条断言恒真（vacuous，review 2026-07-14
//! 实测复现）——所以本测试起隔离 tmux server（镜像 tests/scanner_e2e.rs 的守卫），
//! 开着 scanner 短间隔轮询，等 quota 段真的非空后再做正向断言。
use std::io::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

/// 兜底清理：Drop 时杀掉隔离 tmux server。镜像 tests/scanner_e2e.rs::TmuxKillOnDrop。
struct TmuxKillOnDrop(String);
impl Drop for TmuxKillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-f", "/dev/null", "-L", &self.0, "kill-server"]).output();
    }
}

/// 兜底清理：Drop 时 kill + wait 直接持有的 daemon 子进程。
struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn skip_if_no_tmux() -> bool {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skip: tmux not installed");
        return true;
    }
    false
}

#[test]
fn default_config_spawns_no_quota_fetcher_and_no_alerts() {
    if skip_if_no_tmux() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tfa.sock");
    // transcript 目录隔离：空目录，scanner 绝不读开发机真实 ~/.claude/projects
    // （否则 pane cwd 命中真实项目目录时会绑上真实 transcript）。
    let projects_dir = dir.path().join("claude-projects");
    std::fs::create_dir_all(&projects_dir).unwrap();

    // 隔离 tmux server：真实 pane 让 scanner tick 的 list_panes/liveness 走通
    // （tick 不跑 = QuotaCache 恒空 = 断言 vacuous，见文件头注释）。
    let tmux_sock = format!("tfa-quota-e2e-{}", std::process::id());
    let out = Command::new("tmux")
        .args(["-f", "/dev/null", "-L", &tmux_sock, "new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .output()
        .expect("failed to start isolated tmux server");
    assert!(out.status.success(), "tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr));
    let _tmux_guard = TmuxKillOnDrop(tmux_sock.clone());
    let pane_out = Command::new("tmux")
        .args(["-L", &tmux_sock, "list-panes", "-t", "main", "-F", "#{pane_id}"])
        .output()
        .expect("tmux list-panes failed to run");
    let pane = String::from_utf8(pane_out.stdout).unwrap().trim().to_string();
    assert!(!pane.is_empty(), "no pane id from isolated tmux session");

    let daemon = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_CONFIG_PATH", dir.path().join("nonexistent.toml")) // 缺失文件 = 全默认（real=false）
        .env("TFA_TMUX_SOCKET", &tmux_sock)
        .env("TFA_CLAUDE_PROJECTS_DIR", &projects_dir)
        .env("TFA_SCAN_INTERVAL_MS", "300") // scanner 开着且加速——tick 才会调 QuotaCache::refresh
        .env("TFA_NO_NOTIFY", "1")
        .arg("daemon")
        .spawn()
        .unwrap();
    let _g = DaemonGuard(daemon);
    let start = Instant::now();
    while !sock.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "daemon never created its socket");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 注入一个 claude 会话（真实 pane）：下一轮 tick 的 burn 采样把 claude provider
    // 建进 QuotaCache（consumed=0 也建档——BurnSampler::sample 把本轮 sessions 出现
    // 的 provider 全部纳入 total，QuotaCache::refresh 按 providers() 逐一产出条目）。
    let mut hook = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .env("TFA_SOCKET", &sock)
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1")
        .env("TMUX_PANE", &pane)
        .args(["hook", "claude", "user-prompt-submit"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(b"{}").unwrap();
    assert!(hook.wait().unwrap().success());

    // 轮询直到 quota 段【非空】——只有非空后逐条断言才有内容（空数组上循环体
    // 从不执行，等于什么都没测）。
    let list = || -> serde_json::Value {
        let out = Command::new(env!("CARGO_BIN_EXE_tfa"))
            .env("TFA_SOCKET", &sock)
            .env("TFA_STATE_DIR", dir.path())
            .env("TFA_NO_SPAWN", "1")
            .arg("list")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let quota = loop {
        let json = list();
        let arr = json["quota"].as_array().cloned().unwrap_or_default();
        if !arr.is_empty() {
            break arr;
        }
        assert!(Instant::now() < deadline, "quota section never populated; last: {json}");
        std::thread::sleep(Duration::from_millis(100));
    };
    // 正向断言（强于 != "real_api"）：默认关下每个条目必须是本地估算语义。
    for q in &quota {
        assert_eq!(q["source"], "local_estimate", "默认关时必须是 local_estimate: {q}");
        assert!(q["window_5h_percent"].is_null(), "本地估算绝不编造百分比: {q}");
    }
    // sink 里不得有 quota_alert
    let sink = std::fs::read_to_string(dir.path().join("notify-sink.jsonl")).unwrap_or_default();
    assert!(!sink.contains("quota_alert"), "sink: {sink}");
}
