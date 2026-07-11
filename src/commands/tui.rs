//! `tfa tui` 入口：终端生命周期 + 主事件循环（薄）。
//!
//! ⚠ no-async 纪律（spec §10）：本模块及整个 tui 子系统禁止 tokio /
//! futures / async-std / crossterm `EventStream`。刷新用轮询模型
//! （event::poll + mpsc try_recv），见 spec §9。tests/no_async_gate.rs 是门禁。

use crate::tui::model::{Action, Model};
use crate::tui::poll::{self, PollMsg};
use crate::tui::view;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::sync::mpsc;
use std::time::Duration;

const EVENT_POLL: Duration = Duration::from_millis(150);

/// README 同步维护同样两条（Task 6）；TFA_CLIENT 注入是多 client 下跳对的承重配置。
const KEYBINDINGS: &str = r##"# ~/.tmux.conf — tfa tui 推荐键位
# 注意：display-popup/split-window 的 -e 不做 format 展开（tmux 3.7b 实测），
# 必须用 run-shell 包装，让 #{client_tty} 在按键时先展开成真实 tty 再注入。
# popup（按需查看；需 tmux >= 3.2）：prefix+a 弹出，q/Esc 关闭，Enter 跳转后自动关闭
bind a run-shell -b "tmux display-popup -c '#{client_tty}' -e TFA_CLIENT='#{client_tty}' -E -w 90% -h 80% 'tfa tui'"
# 侧栏（任意 tmux 版本）：prefix+A 打开；Enter 跳转后侧栏关闭
bind A run-shell -b "tmux split-window -h -l 40% -e TFA_CLIENT='#{client_tty}' 'tfa tui'"
"##;

pub fn run(print_keybindings: bool) {
    if print_keybindings {
        print!("{KEYBINDINGS}");
        return;
    }
    spawn_signal_guard();
    let in_tmux = std::env::var_os("TMUX").is_some();
    let tfa_client = crate::tui::nav::sanitize_client(std::env::var("TFA_CLIENT").ok());
    let (tx, rx) = mpsc::channel();
    poll::spawn(tx);
    let mut model = Model::new(in_tmux);
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut model, &rx, tfa_client.as_deref());
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("tfa tui: {e}");
        std::process::exit(1);
    }
}

/// 信号兜底（spec §8）：SIGTERM/SIGHUP/SIGINT → 恢复终端 + 退出，和 `q` 同路径。
/// 防御性加固而非承重：tui 跑在 tmux 独立 pty 里，pane 销毁时 pty 随之消亡，
/// 转义序列不会泄漏到外层真实终端。SIGKILL 无法捕获，已知残余风险。
fn spawn_signal_guard() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP, SIGINT]) {
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                ratatui::restore();
                std::process::exit(130);
            }
        });
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    model: &mut Model,
    rx: &mpsc::Receiver<PollMsg>,
    tfa_client: Option<&str>,
) -> anyhow::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| view::draw(f, model))?;
            dirty = false;
        }
        if event::poll(EVENT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match model.handle_key(key) {
                        Action::Quit => return Ok(()),
                        Action::Redraw => dirty = true,
                        Action::Navigate(pane_id) => {
                            match crate::tui::nav::navigate(&pane_id, tfa_client) {
                                // 跳转成功 → 主动退出进程（popup 的 -E 不因
                                // switch-client 自动关闭，必须自己退，spec §7.3）
                                Ok(()) => return Ok(()),
                                Err(_) => {
                                    model.nav_error = Some("该会话已结束，刷新中…".into());
                                    dirty = true;
                                }
                            }
                        }
                        Action::None => {}
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        // 只取最新、丢弃积压（spec §5：避免 UI 落后于 daemon）
        let mut latest = None;
        while let Ok(msg) = rx.try_recv() {
            latest = Some(msg);
        }
        if let Some(msg) = latest {
            if model.apply_msg(msg) {
                dirty = true;
            }
        }
    }
}
