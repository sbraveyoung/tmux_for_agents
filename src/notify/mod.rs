pub mod channels;

use crate::config::Config;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] // Eq+Hash：Discipline 用作 HashMap 键
pub enum NotifyKind {
    WaitingInput,
    #[allow(dead_code)] // Task 6 discipline fires this trigger
    Done,
    #[allow(dead_code)] // Task 6 discipline fires this trigger
    Stale,
    #[allow(dead_code)] // Task 6 discipline fires this trigger
    Dead,
}
impl NotifyKind {
    pub fn as_str(&self) -> &'static str {
        match self { Self::WaitingInput => "waiting_input", Self::Done => "done", Self::Stale => "stale", Self::Dead => "dead" }
    }
}

#[derive(Debug, Clone)]
pub struct NotifyEvent {
    #[allow(dead_code)] // read by Task 6 discipline's cooldown/generation bookkeeping
    pub session_key: String,
    pub pane_id: String,
    pub session_name: Option<String>,
    pub kind: NotifyKind,
    #[allow(dead_code)] // read by Task 6 discipline's edge-detection bookkeeping
    pub generation: u64,
    pub title: String,
    pub body: String,
}

/// 唯一消费队列的独立线程：串行派发，通道 IO 各带超时，绝不阻塞其它线程。
#[allow(dead_code)] // wired into the daemon by Task 6 (discipline is the mpsc producer)
pub fn spawn_notifier(rx: Receiver<NotifyEvent>, cfg: Arc<Mutex<Config>>) {
    std::thread::spawn(move || {
        for ev in rx {
            let c = { cfg.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() };
            if !c.notify.enabled { continue; }
            channels::dispatch(&ev, &c.notify);
        }
    });
}
