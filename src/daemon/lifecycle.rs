use crate::paths;
use crate::state::StateStore;
use std::fs::File;
use std::sync::Mutex;

/// 返回持有的锁 guard；None 表示已有 daemon 在跑。
/// 泄漏 File 换 'static 生命周期：daemon 进程整个生命周期都要持锁，进程退出自动释放。
pub fn acquire_lock() -> Option<fd_lock::RwLockWriteGuard<'static, File>> {
    let path = paths::lock_path();
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = File::create(&path).ok()?;
    let lock: &'static mut fd_lock::RwLock<File> =
        Box::leak(Box::new(fd_lock::RwLock::new(file)));
    lock.try_write().ok()
}

pub fn tmux_alive() -> bool {
    if std::env::var("TFA_SKIP_TMUX_CHECK").as_deref() == Ok("1") {
        return true;
    }
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(paths::tmux_args()).arg("has-session");
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

pub fn write_snapshot(store: &Mutex<StateStore>) {
    let json = store.lock().unwrap().to_json();
    let path = paths::snapshot_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path); // 原子替换，避免半写快照
    }
}

pub fn check_interval_ms() -> u64 {
    std::env::var("TFA_TMUX_CHECK_INTERVAL_MS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(10_000)
}
