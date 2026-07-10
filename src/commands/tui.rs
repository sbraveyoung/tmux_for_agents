//! `tfa tui` 入口：终端生命周期（init/restore + panic hook）+ 最小事件循环。
//!
//! ⚠ no-async 纪律（spec §10）：本模块及整个 tui 子系统禁止 tokio /
//! futures / async-std / crossterm `EventStream`。刷新用轮询模型
//! （event::poll + mpsc try_recv），见 spec §9。tests/no_async_gate.rs 是门禁。

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::Paragraph;
use ratatui::DefaultTerminal;
use std::time::Duration;

pub fn run() {
    // ratatui::init(): alternate screen + raw mode + panic hook（先恢复终端再打印 panic）
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal);
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("tfa tui: {e}");
        std::process::exit(1);
    }
}

fn event_loop(terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| {
                f.render_widget(Paragraph::new("tfa tui — q 退出"), f.area());
            })?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(150))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // raw mode 下 Ctrl-C 是键盘事件不是 SIGINT（termios ISIG 已关）——
                    // 必须显式给退出分支，否则按 Ctrl-C 毫无反应（spec §8）。
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
}
