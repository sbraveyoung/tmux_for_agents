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

pub fn run() {
    let (tx, rx) = mpsc::channel();
    poll::spawn(tx);
    let mut model = Model::new();
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut model, &rx);
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("tfa tui: {e}");
        std::process::exit(1);
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    model: &mut Model,
    rx: &mpsc::Receiver<PollMsg>,
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
