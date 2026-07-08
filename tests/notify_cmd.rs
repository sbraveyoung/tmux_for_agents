use assert_cmd::cargo::cargo_bin;
use std::process::Command;

#[test]
fn notify_test_under_no_notify_exits_zero() {
    // tfa notify test 在 TFA_NO_NOTIFY=1 下不真弹通知，正常 exit 0
    let out = Command::new(cargo_bin("tfa"))
        .args(["notify", "test"])
        .env("TFA_NO_NOTIFY", "1")
        .env("TFA_CONFIG_PATH", "/nonexistent-config.toml") // 用默认配置
        .output().unwrap();
    assert!(out.status.success(), "notify test 应 exit 0；stderr={}", String::from_utf8_lossy(&out.stderr));
}
