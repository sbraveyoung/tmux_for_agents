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

/// Root directory holding claude's per-project transcript directories
/// (`<projects_dir>/<encode_cwd(cwd)>/*.jsonl`).
pub fn projects_dir() -> PathBuf {
    env_path("TFA_CLAUDE_PROJECTS_DIR").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".claude/projects")
    })
}

/// tmux 调用的额外参数（隔离测试用 -L <name>）
pub fn tmux_args() -> Vec<String> {
    match std::env::var("TFA_TMUX_SOCKET") {
        Ok(name) if !name.is_empty() => vec!["-L".into(), name],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_path_functions() {
        // Single test function to prevent env-var race conditions across parallel test threads.
        // Rust's test runner uses parallel execution; std::env::set_var is process-global.
        // Each assertion sequence must set → assert → remove to avoid interference.

        // Test 1: TFA_SOCKET explicitly set
        std::env::set_var("TFA_SOCKET", "/x/y.sock");
        assert_eq!(socket_path(), PathBuf::from("/x/y.sock"));
        std::env::remove_var("TFA_SOCKET");

        // Test 2: TFA_SOCKET unset → defaults to /tmp/tfa-<uid>/tfa.sock
        assert!(std::env::var("TFA_SOCKET").is_err());
        let path = socket_path();
        assert!(path.to_string_lossy().starts_with("/tmp/tfa-"));
        assert!(path.to_string_lossy().ends_with("/tfa.sock"));

        // Test 3: TFA_STATE_DIR explicitly set
        std::env::set_var("TFA_STATE_DIR", "/s");
        assert_eq!(state_dir(), PathBuf::from("/s"));
        std::env::remove_var("TFA_STATE_DIR");

        // Test 4: TFA_STATE_DIR unset → defaults to $HOME/.local/state/tfa
        let dir = state_dir();
        assert!(dir.to_string_lossy().ends_with(".local/state/tfa"));

        // Test 5: snapshot_path ends with snapshot.json
        let snap = snapshot_path();
        assert!(snap.to_string_lossy().ends_with("snapshot.json"));

        // Test 6: lock_path ends with daemon.lock
        let lock = lock_path();
        assert!(lock.to_string_lossy().ends_with("daemon.lock"));

        // Test 7: TFA_TMUX_SOCKET="abc" → ["-L", "abc"]
        std::env::set_var("TFA_TMUX_SOCKET", "abc");
        assert_eq!(tmux_args(), vec!["-L", "abc"]);
        std::env::remove_var("TFA_TMUX_SOCKET");

        // Test 8: TFA_TMUX_SOCKET unset → empty vec
        assert!(std::env::var("TFA_TMUX_SOCKET").is_err());
        assert_eq!(tmux_args(), Vec::<String>::new());

        // Test 9: TFA_TMUX_SOCKET="" (empty string) → empty vec
        std::env::set_var("TFA_TMUX_SOCKET", "");
        assert_eq!(tmux_args(), Vec::<String>::new());
        std::env::remove_var("TFA_TMUX_SOCKET");

        // Test 10: TFA_CLAUDE_PROJECTS_DIR explicitly set
        std::env::set_var("TFA_CLAUDE_PROJECTS_DIR", "/p/projects");
        assert_eq!(projects_dir(), PathBuf::from("/p/projects"));
        std::env::remove_var("TFA_CLAUDE_PROJECTS_DIR");

        // Test 11: TFA_CLAUDE_PROJECTS_DIR unset → defaults to $HOME/.claude/projects
        assert!(std::env::var("TFA_CLAUDE_PROJECTS_DIR").is_err());
        let projects = projects_dir();
        assert!(projects.to_string_lossy().ends_with(".claude/projects"));
    }
}
