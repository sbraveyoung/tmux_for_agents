use std::path::PathBuf;

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn socket_path() -> PathBuf {
    env_path("TFA_SOCKET").unwrap_or_else(|| {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/tfa-{uid}/tfa.sock"))
    })
}

pub fn state_dir() -> PathBuf {
    env_path("TFA_STATE_DIR").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".local/state/tfa")
    })
}

pub fn snapshot_path() -> PathBuf { state_dir().join("snapshot.json") }
pub fn lock_path() -> PathBuf { state_dir().join("daemon.lock") }

/// tmux 调用的额外参数（隔离测试用 -L <name>）
pub fn tmux_args() -> Vec<String> {
    match std::env::var("TFA_TMUX_SOCKET") {
        Ok(name) if !name.is_empty() => vec!["-L".into(), name],
        _ => vec![],
    }
}
