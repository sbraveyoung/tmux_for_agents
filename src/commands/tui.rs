//! `tfa tui` 入口：终端生命周期 + 主事件循环（薄）。
//!
//! ⚠ no-async 纪律（spec §10）：本模块及整个 tui 子系统禁止 tokio /
//! futures / async-std / crossterm `EventStream`。刷新用轮询模型
//! （event::poll + mpsc try_recv），见 spec §9。tests/no_async_gate.rs 是门禁。

use crate::tui::i18n::{self, Texts};
use crate::tui::model::{Action, Model, NavError};
use crate::tui::poll::{self, PollMsg};
use crate::tui::view;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::sync::mpsc;
use std::time::Duration;

const EVENT_POLL: Duration = Duration::from_millis(150);

/// README（README.md + README.zh-CN.md）同步维护同样两条 bind 行（Task 6）；
/// TFA_CLIENT 注入是多 client 下跳对的承重配置。注释行英文在前、中文在后
/// （2026-07-12 双语发布任务）——两条 `bind` 命令行本身字节不变，const/两份
/// README/spec §11 的引用必须保持一致。
const KEYBINDINGS: &str = r##"# ~/.tmux.conf — recommended tfa tui keybindings
# ~/.tmux.conf — tfa tui 推荐键位
# Note: display-popup/split-window's -e does not expand tmux formats (verified on tmux 3.7b);
# 注意：display-popup/split-window 的 -e 不做 format 展开（tmux 3.7b 实测），
# you must wrap with run-shell so #{client_tty} expands to a real tty before injection.
# 必须用 run-shell 包装，让 #{client_tty} 在按键时先展开成真实 tty 再注入。
# popup (on demand; needs tmux >= 3.2): prefix+a opens it, q/Esc closes, Enter-jump auto-closes it
# popup（按需查看；需 tmux >= 3.2）：prefix+a 弹出，q/Esc 关闭，Enter 跳转后自动关闭
bind a run-shell -b "tmux display-popup -c '#{client_tty}' -t '#{pane_id}' -e TFA_CLIENT='#{client_tty}' -E -w 90% -h 80% 'tfa tui'"
# sidebar (needs tmux >= 3.1): prefix+A opens it; --stay keeps it resident after Enter jumps (the jump already happened; the original window keeps refreshing)
# 侧栏（需 tmux >= 3.1）：prefix+A 打开；--stay 让 Enter 跳转后侧栏常驻（跳转已发生，原窗口继续刷新）
bind A run-shell -b "tmux split-window -t '#{pane_id}' -h -l 40% -e TFA_CLIENT='#{client_tty}' 'tfa tui --stay'"
"##;

/// `[tui] lang` 之外的第二输入源：按优先级取第一个非空的 `LC_ALL` /
/// `LC_MESSAGES` / `LANG`（glibc/POSIX locale 环境变量的标准优先级顺序）。
/// 只在这里读一次（启动时），运行期不追踪变化——同 `[tui]` 颜色配置的
/// 「只在启动时读一次」约定（见 `run()` 里 `resolve_state_styles` 的注释）。
fn env_lang() -> Option<String> {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

pub fn run(print_keybindings: bool, stay: bool) {
    if print_keybindings {
        print!("{KEYBINDINGS}");
        return;
    }
    spawn_signal_guard();
    let in_tmux = std::env::var_os("TMUX").is_some();
    let tfa_client = crate::tui::nav::sanitize_client(std::env::var("TFA_CLIENT").ok());
    // 复用 daemon 同一条 config 加载路径（缺文件/坏 TOML → 默认，绝不硬失败）；
    // 只在启动时读一次，[tui] 颜色配置在运行期不热重载（Part2c 用户验收）。
    let cfg = crate::config::Config::load();
    let styles = view::resolve_state_styles(&cfg.tui);
    // 语言同样只在启动时解析一次（不运行期热切换）：config 显式值优先，否则按
    // LC_ALL/LC_MESSAGES/LANG 探测（i18n 任务 2026-07-12，见 tui::i18n::resolve_lang）。
    let lang = i18n::resolve_lang(&cfg.tui.lang, env_lang().as_deref());
    let texts = i18n::texts(lang);
    let (tx, rx) = mpsc::channel();
    poll::spawn(tx);
    let mut model = Model::new(in_tmux);
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut model, &rx, tfa_client.as_deref(), stay, &styles, texts);
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
    stay: bool,
    styles: &view::StateStyles,
    texts: &Texts,
) -> anyhow::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| view::draw(f, model, styles, texts))?;
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
                                // 跳转成功：popup 必须主动退出（-E 不因 switch-client
                                // 自动关闭，spec §7.3）；--stay（侧栏模式）则不退出——
                                // 跳转已经发生，留在原 window 继续刷新，用户按 A 再关。
                                Ok(()) => {
                                    if stay {
                                        // 常驻后上一次失败的「该会话已结束」提示会残留
                                        // 至下一次快照（≤1s）；成功跳转应立刻清掉。
                                        model.nav_error = None;
                                        dirty = true;
                                    } else {
                                        return Ok(());
                                    }
                                }
                                Err(_) => {
                                    model.nav_error = Some(NavError::TargetGone);
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
