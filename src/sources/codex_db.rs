use crate::state::{SessionMetrics, TokenTotals};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct CodexThread {
    pub cwd: String,
    pub model: Option<String>,
    pub tokens_used: u64,
    pub updated_at_ms: u64,
    #[allow(dead_code)]
    pub title: String,
}

pub fn db_path() -> PathBuf {
    match std::env::var_os("TFA_CODEX_DB").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".codex/state_5.sqlite")
        }
    }
}

pub fn load_recent(limit: usize) -> Vec<CodexThread> {
    let path = db_path();
    if !path.exists() { return Vec::new(); }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT cwd, model, tokens_used, COALESCE(updated_at_ms, updated_at * 1000), title
         FROM threads WHERE archived = 0
         ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC LIMIT ?1",
    ) else { return Vec::new() };
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(CodexThread {
            cwd: r.get(0)?,
            model: r.get(1)?,
            tokens_used: r.get::<_, i64>(2)?.max(0) as u64,
            updated_at_ms: r.get::<_, i64>(3)?.max(0) as u64,
            title: r.get(4)?,
        })
    });
    match rows {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn metrics_for(threads: &[CodexThread], cwd: &str) -> Option<SessionMetrics> {
    let t = threads.iter().filter(|t| t.cwd == cwd).max_by_key(|t| t.updated_at_ms)?;
    Some(SessionMetrics {
        model: t.model.clone(),
        context: None,
        tokens: Some(TokenTotals { total: t.tokens_used, ..Default::default() }),
        git_branch: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 照抄本机 state_5.sqlite 的 threads 建表语句（列子集含全部被查询列）
    fn fixture_db(rows: &[(&str, &str, &str, i64, i64, &str)]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(f.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL DEFAULT '', model_provider TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
                sandbox_policy TEXT NOT NULL DEFAULT '', approval_mode TEXT NOT NULL DEFAULT '',
                tokens_used INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0,
                model TEXT, updated_at_ms INTEGER
            );",
        ).unwrap();
        for (id, cwd, model, tokens, updated_ms, title) in rows {
            conn.execute(
                "INSERT INTO threads (id, cwd, model, tokens_used, updated_at_ms, title) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![id, cwd, model, tokens, updated_ms, title],
            ).unwrap();
        }
        f
    }

    #[test]
    fn codex_db_reader_suite() {
        // Test 1: load_recent_reads_fixture_readonly
        let f = fixture_db(&[
            ("t1", "/proj/a", "gpt-5.3-codex", 1234, 1000, "old"),
            ("t2", "/proj/a", "gpt-5.3-codex", 9999, 2000, "new"),
            ("t3", "/proj/b", "gpt-5.3-codex", 42, 1500, "other"),
        ]);
        std::env::set_var("TFA_CODEX_DB", f.path());
        let threads = load_recent(50);
        std::env::remove_var("TFA_CODEX_DB");
        assert_eq!(threads.len(), 3);

        let m = metrics_for(&threads, "/proj/a").unwrap();
        assert_eq!(m.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(m.tokens.unwrap().total, 9999, "同 cwd 取 updated_at_ms 最新");
        assert!(m.context.is_none());
        assert!(metrics_for(&threads, "/nope").is_none());

        // Test 2: missing_db_and_empty_table_are_graceful
        std::env::set_var("TFA_CODEX_DB", "/nonexistent/state.sqlite");
        assert!(load_recent(10).is_empty());
        std::env::remove_var("TFA_CODEX_DB");
        let f = fixture_db(&[]);
        std::env::set_var("TFA_CODEX_DB", f.path());
        assert!(load_recent(10).is_empty()); // 本机现状：0 行
        std::env::remove_var("TFA_CODEX_DB");
    }
}
