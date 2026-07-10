//! Enter 跳转（spec §7）：唯一的 tmux 写操作。导航（switch-client）不是
//! send-keys——等价于用户自己按 prefix+s，不注入按键、不接管 agent。

use std::process::{Command, Stdio};

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

/// 执行跳转。stdio 全捕获（spec §7.4：tmux 打到 stderr 的任何输出都会糊进
/// raw mode + alternate screen 把界面搞花，绝不继承）。
/// Ok(()) → 调用方退出 TUI（popup 随之关闭）；Err → 目标可能已死（≤1s 陈旧
/// 窗口），调用方留在 TUI 报错，等下一次快照自然纠正。
pub fn navigate(pane_id: &str, tfa_client: Option<&str>) -> Result<(), String> {
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
}
