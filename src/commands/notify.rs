use crate::config::Config;
use crate::notify::{channels, NotifyEvent, NotifyKind};

/// `tfa notify test`：向所有 enabled 通道发一条测试通知（TFA_NO_NOTIFY 下走 sink）。
/// `tfa notify send`：由 daemon 内部使用；CLI 保留最小实现（读默认 config 发一条）。
pub fn run(sub: &str) {
    let cfg = Config::load();
    let ev = NotifyEvent {
        session_key: "test".into(), pane_id: "%0".into(), session_name: Some("tfa".into()),
        kind: NotifyKind::WaitingInput, generation: 0,
        title: "tfa 通知测试".into(),
        body: match sub { "test" => "如果你看到这条，通知通道工作正常".into(), _ => "notify".into() },
    };
    channels::dispatch(&ev, &cfg.notify);
    std::process::exit(0);
}
