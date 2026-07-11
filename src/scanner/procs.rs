use crate::event::AgentKind;
use crate::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub pane_pid: u32,
    pub cwd: String,
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

pub fn parse_panes(out: &str) -> Vec<PaneInfo> {
    out.lines()
        .filter_map(|line| {
            // 尾部两个数值列（window_index/pane_index）从右往左剥离：
            // session_name 处于中间位置后，名字里若含字面 tab，从左 splitn 会
            // 让字段整体右移、u32 解析失败、整行被丢——该 pane 从本轮 live 表
            // 消失，reconcile_liveness 会把它的会话每轮误判 Dead。右剥离让
            // session_name 恢复「实际上的最后一个变长字段」健壮性（splitn(4)
            // 的最后一段吃掉剩余，与坐标列加入前的旧格式等价）。
            let mut tail = line.rsplitn(3, '\t');
            let pane_index: u32 = tail.next()?.parse().ok()?;
            let window_index: u32 = tail.next()?.parse().ok()?;
            let mut it = tail.next()?.splitn(4, '\t');
            let pane_id = it.next()?.to_string();
            let pane_pid: u32 = it.next()?.parse().ok()?;
            let cwd = it.next()?.to_string();
            let session_name = it.next()?.to_string();
            Some(PaneInfo { pane_id, pane_pid, cwd, session_name, window_index, pane_index })
        })
        .collect()
}

pub fn parse_ps(out: &str) -> Vec<ProcEntry> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let ppid: u32 = it.next()?.parse().ok()?;
            let rest = it.collect::<Vec<_>>().join(" ");
            if rest.is_empty() { return None; }
            Some(ProcEntry { pid, ppid, args: rest })
        })
        .collect()
}

pub fn classify(args: &str) -> Option<AgentKind> {
    let argv0 = args.split_whitespace().next()?;
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let is_claude = argv0.contains("/claude/versions/") || base == "claude";
    if is_claude {
        // `claude daemon run` 等常驻进程不是 pane agent
        let is_daemon = args.split_whitespace().nth(1) == Some("daemon");
        return if is_daemon { None } else { Some(AgentKind::Claude) };
    }
    if base == "codex" || base.starts_with("codex-") {
        return Some(AgentKind::Codex);
    }
    None
}

pub fn find_agent(pane_pid: u32, procs: &[ProcEntry]) -> Option<(AgentKind, u32)> {
    // BFS：先看 pane 进程自己，再逐层看子进程，取最浅匹配
    let mut frontier = vec![pane_pid];
    let mut guard = 0;
    while !frontier.is_empty() && guard < 64 {
        guard += 1;
        for &pid in &frontier {
            if let Some(p) = procs.iter().find(|p| p.pid == pid) {
                if let Some(kind) = classify(&p.args) {
                    return Some((kind, pid));
                }
            }
        }
        frontier = procs.iter()
            .filter(|p| frontier.contains(&p.ppid))
            .map(|p| p.pid)
            .collect();
    }
    None
}

/// `None` means the `tmux` command itself failed (couldn't run, or exited
/// non-zero — e.g. no server at the target socket). Callers must not treat
/// that the same as "queried fine, zero panes": tick()'s reconcile_liveness
/// would otherwise mass-mark every live session Dead off a single transient
/// tmux hiccup (F1).
fn list_panes_with_args(extra: &[String]) -> Option<Vec<PaneInfo>> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(extra);
    cmd.args(["list-panes", "-a", "-F", "#{pane_id}\t#{pane_pid}\t#{pane_current_path}\t#{session_name}\t#{window_index}\t#{pane_index}"]);
    match cmd.output() {
        Ok(out) if out.status.success() => Some(parse_panes(&String::from_utf8_lossy(&out.stdout))),
        _ => None,
    }
}

pub fn list_panes() -> Option<Vec<PaneInfo>> {
    list_panes_with_args(&paths::tmux_args())
}

pub fn list_procs() -> Vec<ProcEntry> {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-axo", "pid=,ppid=,args="]);
    match cmd.output() {
        Ok(out) if out.status.success() => parse_ps(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;

    #[test]
    fn parse_panes_splits_tab_fields() {
        let out = "%1\t123\t/Users/u/proj\tcompany\t3\t0\n%22\t456\t/tmp/x y z\tLLM\t0\t2\n";
        let panes = parse_panes(out);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[0].pane_pid, 123);
        assert_eq!(panes[0].window_index, 3);
        assert_eq!(panes[0].pane_index, 0);
        assert_eq!(panes[1].cwd, "/tmp/x y z");
        assert_eq!(panes[1].session_name, "LLM");
        assert_eq!(panes[1].window_index, 0);
        assert_eq!(panes[1].pane_index, 2);
        assert!(parse_panes("garbage without tabs\n").is_empty());
    }

    #[test]
    fn parse_panes_skips_lines_missing_window_pane_index() {
        // 老格式行（缺 window/pane_index 两列，如升级过渡期间的陈旧 tmux 输出
        // 或截断行）视为畸形行，整行跳过而非 panic 或用 0 默认半解析。
        let out = "%1\t123\t/Users/u/proj\tcompany\n";
        assert!(parse_panes(out).is_empty());
    }

    #[test]
    fn parse_panes_preserves_session_name_with_embedded_tab() {
        // session_name 不再是行尾「吃掉剩余」的最后一列后，名字里一个字面 tab
        // 会让从左 splitn 的字段整体右移、u32 解析失败、整行被丢——该 pane 从
        // 本轮 live 表消失，reconcile_liveness 会把它的会话每轮误判 Dead。
        // 尾部两个数值列必须从右往左剥离，session_name 恢复「实际上的最后一个
        // 变长字段」健壮性（与坐标列加入前的旧格式等价）。
        let out = "%1\t123\t/Users/u/proj\tmy\tname\t3\t0\n";
        let panes = parse_panes(out);
        assert_eq!(panes.len(), 1, "tab-in-name line must not be dropped");
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[0].pane_pid, 123);
        assert_eq!(panes[0].cwd, "/Users/u/proj");
        assert_eq!(panes[0].session_name, "my\tname", "name must be preserved whole");
        assert_eq!(panes[0].window_index, 3);
        assert_eq!(panes[0].pane_index, 0);
    }

    #[test]
    fn classify_matches_real_argv_shapes() {
        // 本机实测：claude 的 argv0 是版本路径，不含字面 "claude" 命令名
        assert_eq!(classify("/Users/u/.local/share/claude/versions/2.1.202"), Some(AgentKind::Claude));
        assert_eq!(classify("/Users/u/.local/share/claude/versions/2.1.202 --agent-id x@y --model claude-opus-4-8"), Some(AgentKind::Claude));
        assert_eq!(classify("/Users/u/.local/bin/claude"), Some(AgentKind::Claude));
        assert_eq!(classify("/opt/homebrew/bin/codex"), Some(AgentKind::Codex));
        assert_eq!(classify("/usr/local/Caskroom/codex/0.142.4/codex-x86_64-apple-darwin"), Some(AgentKind::Codex));
        // 排除项
        assert_eq!(classify("/Users/u/.local/bin/claude daemon run --origin transient"), None);
        assert_eq!(classify("tmux-agent-sidebar --collector"), None);
        assert_eq!(classify("-zsh"), None);
        assert_eq!(classify("/bin/zsh -il"), None);
    }

    #[test]
    fn list_panes_with_args_returns_none_on_command_failure() {
        // 指向一个根本不存在的 tmux server：tmux 直接以非零退出连接失败（不会
        // 自动起 server）——list_panes 必须区分「命令失败」和「查询成功但 0
        // 个 pane」，返回 None 而非空 Vec，否则 tick() 会把它误判成「tmux
        // 没有任何 pane」从而对所有会话做批量 liveness 纠偏（F1）。
        // 不设环境变量，直接传参数，避免和其它测试的环境变量竞态。
        assert!(list_panes_with_args(&["-L".into(), "tfa-no-such-server-xyz".into()]).is_none());
    }

    #[test]
    fn find_agent_walks_tree_and_prefers_shallowest() {
        let procs = vec![
            ProcEntry { pid: 100, ppid: 1, args: "/bin/zsh -il".into() },              // pane shell
            ProcEntry { pid: 200, ppid: 100, args: "/U/.local/share/claude/versions/2.1.202".into() },
            ProcEntry { pid: 300, ppid: 200, args: "/U/.local/share/claude/versions/2.1.202 --agent-id sub@x".into() },
            ProcEntry { pid: 999, ppid: 1, args: "/opt/homebrew/bin/codex".into() },   // 别的 pane 的
        ];
        let (kind, pid) = find_agent(100, &procs).unwrap();
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(pid, 200, "取最浅的主进程而非 subagent 子进程");
        assert!(find_agent(999, &procs).is_some(), "pane 进程自身就是 agent 也要匹配");
        assert!(find_agent(4242, &procs).is_none());
    }
}
