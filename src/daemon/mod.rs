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
    let sock_path = paths::socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(paths::state_dir())?;
    let _ = std::fs::remove_file(&sock_path); // 遗留 socket 清理（flock 保证无双活，Task 5）

    let store = Arc::new(Mutex::new(load_or_default()));
    let dirty = Arc::new(AtomicBool::new(false));

    let listener = UnixListener::bind(&sock_path)?;
    server::serve(listener, store, dirty); // 阻塞 accept 循环
    Ok(())
}

fn load_or_default() -> StateStore {
    std::fs::read_to_string(paths::snapshot_path())
        .ok()
        .and_then(|s| StateStore::from_json(&s).ok())
        .unwrap_or_else(StateStore::new)
}
