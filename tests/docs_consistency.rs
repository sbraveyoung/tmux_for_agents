//! 四份 tmux keybinding 拷贝（`KEYBINDINGS` const / README.md / README.zh-CN.md /
//! spec §11）纯靠人工约定保持字节一致，没有共享数据源——这个测试就是那道护栏：
//! 任何一份漂移，测试红，而不是等用户照抄失效的键位配置才发现。

use std::process::Command;

/// 从一行文本里摘出 `bind a run-shell ...` / `bind A run-shell ...` 命令本体：
/// 从 marker 开始，到该行最后一个 `"` 为止（含）。
///
/// README 的围栏代码块里 marker 就在行首、命令本身也在行尾结束，这个切法
/// 退化成整行；spec §11 是内联在项目符号句子里、外面包着 markdown 反引号，
/// 这个切法照样能把命令本体单独摘出来——命令里唯一的两个双引号就是
/// `-b "..."` 的开合括号，从 marker 切到最后一个 `"`，正好落在收尾双引号上，
/// 不会带上后面的反引号/中文解释。
fn extract_bind_command(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)?;
    let end = line.rfind('"')?;
    (end >= start).then(|| line[start..=end].to_string())
}

/// 在整段文本里找 `bind a` / `bind A` 各一行。找不到直接 panic（带上 source
/// 名字方便定位是哪份拷贝缺了绑定）——护栏测试宁可炸得明显，也不要静默跳过
/// 对比、把一份「压根没找到」误判成「一致」。
fn find_bind_lines(text: &str, source: &str) -> (String, String) {
    let mut bind_a = None;
    let mut bind_cap_a = None;
    for line in text.lines() {
        if bind_a.is_none() {
            bind_a = extract_bind_command(line, "bind a run-shell");
        }
        if bind_cap_a.is_none() {
            bind_cap_a = extract_bind_command(line, "bind A run-shell");
        }
    }
    (
        bind_a.unwrap_or_else(|| panic!("{source}: no `bind a run-shell ...` line found")),
        bind_cap_a.unwrap_or_else(|| panic!("{source}: no `bind A run-shell ...` line found")),
    )
}

#[test]
fn keybindings_block_is_identical_across_const_and_docs() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // 唯一权威源：真的跑一次二进制，把 KEYBINDINGS const 打出来
    // （--print-keybindings 在 ratatui::init 之前就 return，不碰终端，可以
    // 放心 spawn，同 tests/cli.rs::tui_print_keybindings_outputs_both_bindings
    // 的用法）。不直接读 src/commands/tui.rs 源码字符串，避免测试自己重新
    // 实现一遍「解析 Rust 源码里的 raw string」——跑二进制才是唯一权威。
    let out = Command::new(env!("CARGO_BIN_EXE_tfa"))
        .args(["tui", "--print-keybindings"])
        .output()
        .expect("spawn `tfa tui --print-keybindings`");
    assert!(out.status.success(), "--print-keybindings exited non-zero");
    let const_output = String::from_utf8(out.stdout).expect("--print-keybindings stdout is not utf8");
    let (const_a, const_cap_a) = find_bind_lines(&const_output, "KEYBINDINGS const (tfa tui --print-keybindings)");

    let docs = [
        "README.md",
        "README.zh-CN.md",
        "docs/superpowers/specs/2026-07-10-tfa-tui-design.md",
    ];
    for doc in docs {
        let path = format!("{manifest_dir}/{doc}");
        let content = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let (doc_a, doc_cap_a) = find_bind_lines(&content, doc);
        assert_eq!(doc_a, const_a, "{doc}: `bind a` line diverged from KEYBINDINGS const");
        assert_eq!(doc_cap_a, const_cap_a, "{doc}: `bind A` line diverged from KEYBINDINGS const");
    }
}
