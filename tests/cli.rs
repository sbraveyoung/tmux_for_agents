use assert_cmd::Command;

#[test]
fn help_lists_subcommands() {
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    let assert = cmd.arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in ["daemon", "hook", "status", "list", "tui"] {
        assert!(out.contains(sub), "missing subcommand {sub}");
    }
}

#[test]
fn status_without_daemon_reports_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("tfa").unwrap();
    cmd.env("TFA_SOCKET", dir.path().join("no.sock"))
        .env("TFA_STATE_DIR", dir.path())
        .env("TFA_NO_SPAWN", "1") // 测试用：禁止自动拉起
        .args(["status", "--format", "tmux"]);
    cmd.assert().success().stdout(predicates::str::contains("tfa:off"));
}
