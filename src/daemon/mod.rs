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
        ensure_socket_dir(parent)?;
    }
    let _ = std::fs::remove_file(&sock_path); // 持锁后清理遗留 socket 是安全的

    let store = Arc::new(Mutex::new(load_or_default()));
    let dirty = Arc::new(AtomicBool::new(false));
    let quota = Arc::new(Mutex::new(crate::quota::QuotaCache::new()));

    // 通知管道：config 加载一次、常驻 notifier 线程消费 mpsc，Discipline 用快照恢复的
    // 既有会话播种基线（避免快照重启把「早已在等待」的旧会话误判成新边沿重复发送）。
    let config = Arc::new(Mutex::new(crate::config::Config::load()));
    let (notify_tx, notify_rx) = std::sync::mpsc::channel::<crate::notify::NotifyEvent>();
    crate::notify::spawn_notifier(notify_rx, Arc::clone(&config));
    let discipline = Arc::new(Mutex::new({
        let boot = config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).notify.discipline.boot_grace_secs;
        let mut d = crate::notify::discipline::Discipline::new(boot, now_ms());
        d.seed(&store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions());
        d
    }));

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
                store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).prune(now_ms());
                lifecycle::clean_activity_markers();
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

    crate::scanner::spawn(
        Arc::clone(&store), Arc::clone(&dirty), Arc::clone(&quota),
        Arc::clone(&config), Arc::clone(&discipline), notify_tx.clone(),
    );

    let listener = UnixListener::bind(&sock_path)?;
    server::serve(listener, store, dirty, quota, config, discipline, notify_tx); // 阻塞 accept 循环
    Ok(())
}

fn load_or_default() -> StateStore {
    std::fs::read_to_string(paths::snapshot_path())
        .ok()
        .and_then(|s| StateStore::from_json(&s).ok())
        .unwrap_or_default()
}

/// socket 父目录必须 0700 且属主是当前用户——多用户共享的 /tmp 或
/// $XDG_RUNTIME_DIR 上，宽松权限会让别的本地用户读到/连上这条控制通道。
/// 用 DirBuilder::mode(0o700) 原子建目录，避免 create_dir_all → chmod
/// 之间目录短暂停留在 umask 权限的 TOCTOU 窗口。
fn ensure_socket_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?; // recursive: 已存在时 Ok，不报错
    // 已存在目录（老版本 create_dir_all 建的）统一收紧到 0700
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let meta = std::fs::metadata(dir)?;
    let uid = unsafe { libc::getuid() };
    anyhow::ensure!(
        meta.uid() == uid,
        "socket dir {} owned by uid {}, expected {}",
        dir.display(), meta.uid(), uid
    );
    Ok(())
}
