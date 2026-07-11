//! scanner 集成测试：隔离 tmux server + 假 claude 进程 + 假 transcript。
//!
//! 假 agent 技巧：真实的 `sleep` 二进制，通过 shell 的 `exec -a <fake-argv0>`
//! 让其 argv[0] 变成一个含 `/claude/versions/` 的路径，从而命中
//! `scanner::procs::classify`。之所以不直接把一个 `#!/bin/sh` 脚本文件路径传给
//! `exec`——实测（见任务报告）在 macOS 上内核会把脚本的 shebang 解释器
//! （`/bin/sh`）重写成 argv[0]，脚本自身路径退居 argv[1]，导致 classify 的
//! argv0 匹配落空。`exec -a` 绕开了这个问题，且不依赖复制/篡改系统二进制
//! （macOS 代码签名会直接 kill 掉复制出来的系统二进制）。用 `bash -c` 包一层
//! 是为了不依赖 tmux pane 的默认 shell 是否支持 `-a`（dash 不支持，bash/zsh
//! 支持）。

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Task 2 (src/sources/claude_jsonl.rs) 测试夹具的原样拷贝：一条真实形状的
/// assistant 行，model=claude-fable-5，percent=98。
const REAL_ASSISTANT_LINE: &str = r#"{"type":"assistant","gitBranch":"main","cwd":"/tmp/p","sessionId":"a5f1915b","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"cache_creation_input_tokens":705,"cache_read_input_tokens":982162,"output_tokens":1045,"service_tier":"standard"}},"uuid":"u1"}"#;

/// src/sources/claude_jsonl.rs::encode_cwd 的原样拷贝——tfa 是纯二进制 crate
/// （无 lib target），集成测试拿不到内部函数，只能按相同规则内联重实现。
fn encode_cwd(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 兜底清理：Drop 时杀掉隔离 tmux server。镜像 tests/e2e.rs::TmuxKillOnDrop。
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

struct TestEnv {
    state_dir: tempfile::TempDir,
    projects_dir: PathBuf,
    sock: PathBuf,
    tmux_sock: String,
    _tmux_guard: TmuxKillOnDrop,
}

impl TestEnv {
    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .args(["-f", "/dev/null", "-L", &self.tmux_sock])
            .args(args)
            .output()
            .expect("tmux command failed to run")
    }

    fn base_envs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("TFA_SOCKET", self.sock.to_string_lossy().into_owned()),
            ("TFA_STATE_DIR", self.state_dir.path().to_string_lossy().into_owned()),
            ("TFA_TMUX_SOCKET", self.tmux_sock.clone()),
            ("TFA_CLAUDE_PROJECTS_DIR", self.projects_dir.to_string_lossy().into_owned()),
        ]
    }

    fn spawn_daemon(&self, extra: &[(&str, &str)]) -> DaemonGuard {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
        for (k, v) in self.base_envs() {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.arg("daemon");
        let child = cmd.spawn().expect("spawn daemon");
        for _ in 0..200 {
            if self.sock.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(self.sock.exists(), "daemon never created its socket");
        DaemonGuard(child)
    }

    /// 直接调 `tfa hook`（daemon 已在跑，TFA_NO_SPAWN=1 防止误触发自动拉起）。
    fn hook(&self, pane: &str, event: &str, payload: &str) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
        for (k, v) in self.base_envs() {
            cmd.env(k, v);
        }
        cmd.env("TFA_NO_SPAWN", "1")
            .env("TMUX_PANE", pane)
            .args(["hook", "claude", event])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn hook");
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "tfa hook exited non-zero");
    }

    /// 返回 `tfa list` 的 `sessions` 数组（M3 起 wire 形状是 `{sessions, quota}` 对象；
    /// 这里解一层，让调用方继续像 M1/M2 时代一样对着裸数组用 `.as_array()`）。
    fn list(&self) -> serde_json::Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tfa"));
        for (k, v) in self.base_envs() {
            cmd.env(k, v);
        }
        cmd.env("TFA_NO_SPAWN", "1").arg("list");
        let out = cmd.output().expect("list command failed to run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("bad list json: {e}; got: {stdout}"));
        v["sessions"].clone()
    }
}

fn setup() -> TestEnv {
    let state_dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmux_sock = format!("tfa-scan-e2e-{}-{}", std::process::id(), n);
    let sock = state_dir.path().join("tfa.sock");
    let projects_dir = state_dir.path().join("claude-projects");
    std::fs::create_dir_all(&projects_dir).unwrap();

    let out = Command::new("tmux")
        .args(["-f", "/dev/null", "-L", &tmux_sock, "new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .output()
        .expect("failed to start isolated tmux server");
    assert!(out.status.success(), "tmux new-session failed: {}", String::from_utf8_lossy(&out.stderr));

    TestEnv {
        state_dir,
        projects_dir,
        sock,
        tmux_sock: tmux_sock.clone(),
        _tmux_guard: TmuxKillOnDrop(tmux_sock),
    }
}

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {timeout_ms}ms");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 在给定路径下放一个占位的 shebang 脚本文件（0755）——纯粹是为了让磁盘布局
/// 看起来像真实的 claude 安装（`.local/share/claude/versions/<ver>`）；这个文件
/// 本身并不会被执行，实际跑起来的是通过 `exec -a` 伪装 argv0 的 `sleep`。
fn write_placeholder_agent_file(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\nsleep 300\n").unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// 往指定 pane 里起一个 argv0 伪装成 `<fake_path>` 的 `sleep 300` 进程。
/// 用 `bash -c` 包一层，不依赖 pane 默认 shell 是否支持 `exec -a`。
fn spawn_fake_agent(env: &TestEnv, target: &str, fake_path: &Path) {
    let cmd = format!("bash -c 'exec -a \"{}\" sleep 300'", fake_path.display());
    assert!(env.tmux(&["send-keys", "-t", target, &cmd, "Enter"]).status.success());
}

fn skip_if_no_tmux() -> bool {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skip: tmux not installed");
        return true;
    }
    false
}

/// 存量场景（sidebar 盲区）：daemon 启动前 pane 和假 claude 进程就已存在，从未
/// 有任何 hook 事件——scanner 必须仅凭 tmux+ps+transcript 扫描把这条会话建档，
/// 并在假进程消失后把它纠偏成 Dead（M1 幽灵 working 状态的根治验证）。
#[test]
fn scanner_backfills_preexisting_agent_and_marks_dead() {
    if skip_if_no_tmux() {
        return;
    }
    let env = setup();

    let fake_path = env.state_dir.path().join("fakehome/.local/share/claude/versions/9.9.9");
    write_placeholder_agent_file(&fake_path);

    let cwd = env.state_dir.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    assert!(env.tmux(&["new-window", "-t", "main", "-c", cwd.to_str().unwrap()]).status.success());

    // 用 tmux 自己报告的 pane_current_path 构造 transcript 目录名，避免临时目录
    // 在不同进程里被不同地解析符号链接（如 macOS /tmp -> /private/tmp）导致的
    // encode_cwd 不一致。
    let real_cwd_out = env.tmux(&["display-message", "-p", "-t", "main:1", "#{pane_current_path}"]);
    let real_cwd = String::from_utf8(real_cwd_out.stdout).unwrap().trim().to_string();
    assert!(!real_cwd.is_empty(), "failed to read pane_current_path");

    // 期望坐标同样以 tmux 自己报告的为准（不硬编码 1/0——window/pane 编号受
    // base-index 等配置影响，发现方式与上面 real_cwd 一致）：验证坐标真的走通
    // 了 tmux list-panes → tick() upsert_scanned → snapshot JSON 全链路。
    let coord_out = env.tmux(&["display-message", "-p", "-t", "main:1", "#{window_index} #{pane_index}"]);
    let coord_str = String::from_utf8(coord_out.stdout).unwrap().trim().to_string();
    let mut coord_it = coord_str.split_whitespace();
    let expect_w: u64 = coord_it.next().expect("window_index missing").parse().expect("window_index not a number");
    let expect_p: u64 = coord_it.next().expect("pane_index missing").parse().expect("pane_index not a number");

    let proj_dir = env.projects_dir.join(encode_cwd(&real_cwd));
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("s1.jsonl"), format!("{REAL_ASSISTANT_LINE}\n")).unwrap();

    spawn_fake_agent(&env, "main:1", &fake_path);

    // daemon 起来之前从未发生过任何 hook 事件；TFA_SCAN_INTERVAL_MS 加速测试。
    let _daemon = env.spawn_daemon(&[("TFA_SCAN_INTERVAL_MS", "300")]);

    wait_until(
        || {
            let sessions = env.list();
            sessions.as_array().is_some_and(|a| {
                a.iter().any(|s| {
                    s["agent"] == "claude"
                        && s["source"] == "scan"
                        && s["state"] == "working"
                        && s["model"] == "claude-fable-5"
                        && s["context"]["percent"] == 98
                        && s["window_index"] == expect_w
                        && s["pane_index"] == expect_p
                })
            })
        },
        5000,
    );

    // 杀掉假 agent 所在窗口 -> liveness 纠偏应把它标记为 Dead（不依赖任何
    // SessionEnd hook）。
    assert!(env.tmux(&["kill-window", "-t", "main:1"]).status.success());
    wait_until(
        || {
            let sessions = env.list();
            sessions.as_array().is_some_and(|a| a.iter().any(|s| s["state"] == "dead"))
        },
        5000,
    );
}

/// hook 报了一个真实存在、且真的跑着假 claude 进程的 pane（source=hook）——
/// scanner 下一轮应把它升级成 Both 并（若尚未有名字）回填 session_name；
/// 同时 hook 报了一个 tmux 里根本不存在的 pane_id——reconcile_liveness 应把
/// 它判 Dead（pane 不在本轮活跃表里 = 已消失）。
#[test]
fn hook_entry_gets_confirmed_to_both_and_ghost_pane_marked_dead() {
    if skip_if_no_tmux() {
        return;
    }
    let env = setup();

    let fake_path = env.state_dir.path().join("fakehome/.local/share/claude/versions/9.9.9");
    write_placeholder_agent_file(&fake_path);

    let out = env.tmux(&["list-panes", "-t", "main", "-F", "#{pane_id}"]);
    let real_pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(!real_pane.is_empty(), "failed to discover real pane id");

    let _daemon = env.spawn_daemon(&[("TFA_SCAN_INTERVAL_MS", "300")]);

    spawn_fake_agent(&env, "main:0", &fake_path);

    // 真实存在的 pane：hook 先落一笔（source=hook）。
    env.hook(&real_pane, "user-prompt-submit", r#"{"prompt":"hi"}"#);
    // 不存在的 pane：hook 也能建档，但 tmux 里根本没有它。
    env.hook("%999", "stop", "{}");

    wait_until(
        || {
            let sessions = env.list();
            let Some(a) = sessions.as_array() else { return false };
            let real_confirmed = a.iter().any(|s| {
                s["pane_id"] == real_pane.as_str() && s["source"] == "both" && s["session_name"].is_string()
            });
            let ghost_dead = a.iter().any(|s| s["pane_id"] == "%999" && s["state"] == "dead");
            real_confirmed && ghost_dead
        },
        5000,
    );
}
