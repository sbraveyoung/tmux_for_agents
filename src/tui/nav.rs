//! Enter 跳转（spec §7）：唯一的 tmux 写操作。导航（switch-client）不是
//! send-keys——等价于用户自己按 prefix+s，不注入按键、不接管 agent。

use std::process::{Command, Stdio};

/// TFA_CLIENT 健全性检查：只接受形如 tty 路径（以 '/' 开头）的值。
/// tmux 3.7b 的 display-popup/split-window `-e` 不做 format 展开——配错的
/// 键位会把字面量 "#{client_tty}" 注入进来；无条件透传给 switch-client -c
/// 会让每次跳转必然失败。不像 tty 就当没注入：降级为不带 -c（单 client 正确）。
pub fn sanitize_client(raw: Option<String>) -> Option<String> {
    raw.filter(|s| s.starts_with('/'))
}

/// 构造跳转 argv（纯函数，可单测）。`;` 作为独立 argv 元素传给 tmux，
/// 不经 shell、无转义问题。完整链一次性 chain：switch-client →
/// select-window → select-pane —— select-pane 不会自动激活所在 window，
/// 必须显式 select-window（spec §7.2）。`-c` 显式注入发起 client：多 client
/// attach 同一 session 时不靠 tmux 隐式推断（spec §7.1）。
pub fn nav_argv(tmux_args: &[String], tfa_client: Option<&str>, pane_id: &str) -> Vec<String> {
    let mut v: Vec<String> = vec!["tmux".into()];
    v.extend(tmux_args.iter().cloned());
    v.push("switch-client".into());
    if let Some(c) = tfa_client {
        v.push("-c".into());
        v.push(c.to_string());
    }
    for part in ["-t", pane_id, ";", "select-window", "-t", pane_id, ";", "select-pane", "-t", pane_id] {
        v.push(part.to_string());
    }
    v
}

/// 执行一次跳转链（无重试）。stdio 全捕获（spec §7.4：tmux 打到 stderr 的任何
/// 输出都会糊进 raw mode + alternate screen 把界面搞花，绝不继承）。
fn run_switch(pane_id: &str, tfa_client: Option<&str>) -> Result<(), String> {
    let argv = nav_argv(&crate::paths::tmux_args(), tfa_client, pane_id);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// 是否应该在首次尝试失败后，退化重试一次不带 `-c`——纯决策函数，从
/// `navigate` 里摘出来是为了不需要真实 tmux 就能单测这四种组合（原先整个
/// 决策埋在 `navigate` 的 match 分支里，只能靠真 tmux 集成测试才能覆盖到）。
/// 只有「注入过 client 且首次尝试确实失败」才重试；没注入 client（`had_client
/// = false`）说明本来就是单次尝试、无 `-c` 可退化；首次已经成功
/// （`first_attempt_failed = false`）自然也不需要重试。语义见下面
/// `navigate` 的 doc comment（spec §7.1「any behavior beats permanently
/// broken」）。
fn should_retry_without_client(had_client: bool, first_attempt_failed: bool) -> bool {
    had_client && first_attempt_failed
}

/// 执行跳转。Ok(()) → 调用方退出 TUI（popup 随之关闭）；Err → 目标可能已死
/// （≤1s 陈旧窗口），调用方留在 TUI 报错，等下一次快照自然纠正。
///
/// **`-c` 失败降级重试一次不带 `-c`（--stay 长驻侧栏场景，2026-07-12 增补）**：
/// `--stay` 侧栏在 spawn 时把发起 client 的 tty 存进 `TFA_CLIENT` 环境变量，此后
/// 整个进程生命周期都复用这一个值——但侧栏是长驻的，期间终端可能 detach/
/// reattach（SSH 断线重连、`tmux attach` 换了个新 tty），存的 tty 就死了。带着
/// 死 tty 的 `-c` 会让 `switch-client` 每次必错，Enter 从此永久失效，还只显示
/// 「该会话已结束，刷新中…」这种误导性提示——目标其实还活着，错在 `-c` 指向的
/// 发起 client，不在目标 pane。缓解：带 `-c` 的首次尝试失败时，退化重试一次不带
/// `-c`——tmux 转而隐式推断当前 client，单 client 场景（绝大多数用户）下这个推断
/// 天然正确；多 client 场景可能切错 client，但比「永久失效」好——参见 spec §7.1
/// 「any behavior beats permanently broken」。只重试一次，不递归、不循环；
/// `tfa_client` 为 `None`（未注入或已被 `sanitize_client` 拒绝）时行为不变——
/// 单次尝试，不重试。重试与否的判断见 `should_retry_without_client`。
pub fn navigate(pane_id: &str, tfa_client: Option<&str>) -> Result<(), String> {
    let first = run_switch(pane_id, tfa_client);
    if should_retry_without_client(tfa_client.is_some(), first.is_err()) {
        run_switch(pane_id, None)
    } else {
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_default_socket_no_client() {
        assert_eq!(
            nav_argv(&[], None, "%37"),
            vec![
                "tmux", "switch-client", "-t", "%37", ";",
                "select-window", "-t", "%37", ";",
                "select-pane", "-t", "%37",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn argv_with_client_injects_dash_c() {
        let v = nav_argv(&[], Some("/dev/ttys004"), "%37");
        let expect: Vec<String> = vec![
            "tmux", "switch-client", "-c", "/dev/ttys004", "-t", "%37", ";",
            "select-window", "-t", "%37", ";",
            "select-pane", "-t", "%37",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(v, expect);
    }

    #[test]
    fn argv_with_isolated_socket_prefixes_dash_l() {
        let v = nav_argv(&["-L".into(), "testsock".into()], None, "%1");
        assert_eq!(&v[..3], &["tmux".to_string(), "-L".into(), "testsock".into()]);
        assert_eq!(v[3], "switch-client");
    }

    #[test]
    fn retries_without_client_only_when_had_client_and_first_attempt_failed() {
        // 四种组合穷举（tech-debt fix，2026-07-13）：之前这个决策埋在
        // navigate 的 match 分支里，没有真实 tmux 就测不到；抽出来之后
        // 纯函数四种组合直接单测，不需要起进程。
        assert!(
            should_retry_without_client(true, true),
            "注入过 client 且首次失败 → 应该退化重试一次不带 -c"
        );
        assert!(
            !should_retry_without_client(true, false),
            "注入过 client 但首次已经成功 → 不该多此一举再跑一次"
        );
        assert!(
            !should_retry_without_client(false, true),
            "没注入 client（None，或被 sanitize_client 拒绝）→ 本来就是单次尝试，没有 -c 可退化"
        );
        assert!(
            !should_retry_without_client(false, false),
            "没 client 且首次成功 → 显然不重试"
        );
    }

    #[test]
    fn sanitize_client_accepts_tty_paths_rejects_garbage() {
        assert_eq!(sanitize_client(Some("/dev/ttys015".into())), Some("/dev/ttys015".to_string()));
        assert_eq!(sanitize_client(Some("#{client_tty}".into())), None, "未展开的 format 字面量必须被拒");
        assert_eq!(sanitize_client(Some("".into())), None);
        assert_eq!(sanitize_client(None), None);
    }
}
