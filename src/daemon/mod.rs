pub mod lifecycle;
pub mod server;

use crate::paths;
use crate::state::StateStore;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn run() -> anyhow::Result<()> {
    std::fs::create_dir_all(paths::state_dir())?;
    let Some(_lock) = lifecycle::acquire_lock() else {
        return Ok(()); // 已有实例，静默退出 0
    };
    let sock_path = paths::socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&sock_path); // 持锁后清理遗留 socket 是安全的

    let store = Arc::new(Mutex::new(load_or_default()));
    let dirty = Arc::new(AtomicBool::new(false));

    // 后台维护线程：快照 + prune（5s）、tmux 存活（可配置，默认 10s）
    {
        let store = Arc::clone(&store);
        let dirty = Arc::clone(&dirty);
        let sock = sock_path.clone();
        std::thread::spawn(move || {
            let check_every = lifecycle::check_interval_ms();
            let mut since_check: u64 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(check_every.min(5000)));
                since_check += check_every.min(5000);
                store.lock().unwrap().prune(now_ms());
                if dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    lifecycle::write_snapshot(&store);
                }
                if since_check >= check_every {
                    since_check = 0;
                    if !lifecycle::tmux_alive() {
                        lifecycle::write_snapshot(&store);
                        let _ = std::fs::remove_file(&sock);
                        std::process::exit(0);
                    }
                }
            }
        });
    }

    crate::scanner::spawn(Arc::clone(&store), Arc::clone(&dirty));

    let listener = UnixListener::bind(&sock_path)?;
    server::serve(listener, store, dirty); // 阻塞 accept 循环
    Ok(())
}

fn load_or_default() -> StateStore {
    std::fs::read_to_string(paths::snapshot_path())
        .ok()
        .and_then(|s| StateStore::from_json(&s).ok())
        .unwrap_or_default()
}
