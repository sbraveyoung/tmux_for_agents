//! 架构不变量保险丝（spec §10）：tfa 全树禁 async runtime。
//! ratatui 官网有篇 async EventStream 教程正是「键盘+定时刷新」场景的诱因，
//! 后来者极易照抄把 tokio 拖进来——靠这条测试而不是 code review 记性。

use std::process::Command;

#[test]
fn dependency_tree_has_no_async_runtime() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["tree", "-e", "normal", "--prefix", "none"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    let tree = String::from_utf8_lossy(&out.stdout);
    for banned in ["tokio", "futures-util", "async-std"] {
        assert!(
            !tree.lines().any(|l| l.trim_start().starts_with(banned)),
            "banned async dependency `{banned}` in cargo tree:\n{tree}"
        );
    }
}
