use crate::state::{ContextUsage, SessionMetrics, TokenTotals};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const TAIL_CAP: u64 = 262_144;

#[derive(Debug, Default, Clone)]
pub struct TranscriptCursor {
    pub offset: u64,
    pub consumed: u64,
}

pub fn context_window(model: &str) -> Option<u64> {
    if model.starts_with("claude-fable") {
        Some(1_000_000)
    } else if model.starts_with("claude-") {
        Some(200_000)
    } else {
        None
    }
}

pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub fn discover_transcript(projects_dir: &Path, cwd: &str) -> Option<PathBuf> {
    let dir = projects_dir.join(encode_cwd(cwd));
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

pub fn read_update(path: &Path, cursor: &mut TranscriptCursor) -> Option<SessionMetrics> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < cursor.offset { cursor.offset = 0; } // truncate/rotate → 重读
    if cursor.offset == len { return None; }
    let mut skip_first_partial = false;
    if cursor.offset == 0 && len > TAIL_CAP {
        cursor.offset = len - TAIL_CAP;
        skip_first_partial = true;
    }
    file.seek(SeekFrom::Start(cursor.offset)).ok()?;
    let mut buf = Vec::with_capacity((len - cursor.offset) as usize);
    file.take(len - cursor.offset).read_to_end(&mut buf).ok()?;

    // 只处理到最后一个完整行；半写行留待下一轮
    let complete_end = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1)?;
    let window = &buf[..complete_end];
    cursor.offset += complete_end as u64;

    let text = String::from_utf8_lossy(window);
    let mut lines = text.lines();
    if skip_first_partial { lines.next(); }

    let mut latest: Option<SessionMetrics> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue }; // 坏行跳过
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") { continue; }
        let Some(msg) = v.get("message") else { continue };
        let Some(model) = msg.get("model").and_then(|m| m.as_str()) else { continue };
        if model == "<synthetic>" { continue; }
        let Some(usage) = msg.get("usage") else { continue };
        let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        // 单调 consumed：只累加新 token（排除 cache_read/cache_creation 避免重复计上下文）
        cursor.consumed = cursor.consumed.saturating_add(g("input_tokens") + g("output_tokens"));
        let tokens = TokenTotals {
            input: g("input_tokens"),
            output: g("output_tokens"),
            cache_read: g("cache_read_input_tokens"),
            cache_creation: g("cache_creation_input_tokens"),
            total: g("input_tokens") + g("output_tokens")
                + g("cache_read_input_tokens") + g("cache_creation_input_tokens"),
        };
        let used = tokens.input + tokens.cache_read + tokens.cache_creation;
        let max = context_window(model);
        let context = ContextUsage {
            used_tokens: used,
            max_tokens: max,
            percent: max.map(|m| ((used.saturating_mul(100)) / m.max(1)).min(100) as u8),
        };
        latest = Some(SessionMetrics {
            model: Some(model.to_string()),
            context: Some(context),
            tokens: Some(tokens),
            git_branch: v.get("gitBranch").and_then(|b| b.as_str()).map(String::from),
        });
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // 形状照抄本机真实行（数值取自 2026-07-07 实测）
    const REAL_ASSISTANT: &str = r#"{"type":"assistant","gitBranch":"main","cwd":"/tmp/p","sessionId":"a5f1915b","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"cache_creation_input_tokens":705,"cache_read_input_tokens":982162,"output_tokens":1045,"service_tier":"standard"}},"uuid":"u1"}"#;
    const SYNTHETIC: &str = r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"uuid":"u2"}"#;
    const HEADER: &str = r#"{"type":"last-prompt","leafUuid":"x","sessionId":"a5f1915b"}"#;

    fn write_lines(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines { writeln!(f, "{l}").unwrap(); }
        f.flush().unwrap();
        f
    }

    #[test]
    fn parses_last_real_assistant_and_skips_noise() {
        let f = write_lines(&[HEADER, "not json at all {", REAL_ASSISTANT, SYNTHETIC]);
        let mut cur = TranscriptCursor::default();
        let m = read_update(f.path(), &mut cur).unwrap();
        assert_eq!(m.model.as_deref(), Some("claude-fable-5"));
        let t = m.tokens.unwrap();
        assert_eq!(t.cache_read, 982162);
        assert_eq!(t.total, 2 + 1045 + 982162 + 705);
        let c = m.context.unwrap();
        assert_eq!(c.used_tokens, 2 + 982162 + 705);
        assert_eq!(c.max_tokens, Some(1_000_000));
        assert_eq!(c.percent, Some(98));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn incremental_read_only_sees_new_lines_and_holds_partial() {
        let f = write_lines(&[REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        assert!(read_update(f.path(), &mut cur).is_some());
        let after_first = cur.offset;
        // 无新内容 → None，offset 不动
        assert!(read_update(f.path(), &mut cur).is_none());
        assert_eq!(cur.offset, after_first);
        // 追加半行（无 \n）→ None，offset 不越过半行
        let mut fh = std::fs::OpenOptions::new().append(true).open(f.path()).unwrap();
        write!(fh, "{}", &REAL_ASSISTANT[..40]).unwrap();
        fh.flush().unwrap();
        assert!(read_update(f.path(), &mut cur).is_none());
        assert_eq!(cur.offset, after_first);
        // 补全该行 + 换行 → 解析成功
        writeln!(fh, "{}", &REAL_ASSISTANT[40..]).unwrap();
        fh.flush().unwrap();
        assert!(read_update(f.path(), &mut cur).is_some());
    }

    #[test]
    fn truncated_file_resets_offset() {
        let f = write_lines(&[REAL_ASSISTANT, REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        std::fs::write(f.path(), format!("{REAL_ASSISTANT}\n")).unwrap(); // 变短
        let m = read_update(f.path(), &mut cur);
        assert!(m.is_some(), "缩短的文件应从头重读");
    }

    #[test]
    fn missing_file_returns_none() {
        let mut cur = TranscriptCursor::default();
        assert!(read_update(std::path::Path::new("/nonexistent/x.jsonl"), &mut cur).is_none());
    }

    #[test]
    fn context_window_table() {
        assert_eq!(context_window("claude-fable-5"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-8"), Some(200_000));
        assert_eq!(context_window("gpt-x"), None);
    }

    #[test]
    fn encode_cwd_matches_observed_layout() {
        assert_eq!(
            encode_cwd("/Users/u/code/tmux_for_agents/.claude/worktrees/calm-fox-mlzj"),
            "-Users-u-code-tmux-for-agents--claude-worktrees-calm-fox-mlzj"
        );
    }

    #[test]
    fn first_read_of_large_file_skips_to_tail_cap() {
        // 文件 > TAIL_CAP：首读只看尾部窗口，丢弃首个不完整行，仍能解出最后一条 assistant
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let filler = format!("{{\"type\":\"filler\",\"pad\":\"{}\"}}", "x".repeat(1024));
        while f.as_file().metadata().unwrap().len() <= TAIL_CAP {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(f, "{REAL_ASSISTANT}").unwrap();
        f.flush().unwrap();
        let mut cur = TranscriptCursor::default();
        let m = read_update(f.path(), &mut cur).unwrap();
        assert_eq!(m.model.as_deref(), Some("claude-fable-5"));
        let len = f.as_file().metadata().unwrap().len();
        assert_eq!(cur.offset, len, "offset should reach EOF after first tail read");
        // 增量续读依旧正常
        writeln!(f, "{REAL_ASSISTANT}").unwrap();
        f.flush().unwrap();
        assert!(read_update(f.path(), &mut cur).is_some());
    }

    #[test]
    fn consumed_accumulates_new_output_and_input_excluding_cache() {
        // REAL_ASSISTANT: input_tokens=2, output_tokens=1045, cache_read=982162, cache_creation=705
        // consumed delta = input+output = 1047，排除 cache_read/cache_creation
        let f = write_lines(&[REAL_ASSISTANT, REAL_ASSISTANT]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, 1047 * 2, "两条真实 assistant 各累加 input+output=1047");
    }

    #[test]
    fn consumed_skips_synthetic_and_is_monotonic_across_incremental_reads() {
        let f = write_lines(&[REAL_ASSISTANT, SYNTHETIC, HEADER]);
        let mut cur = TranscriptCursor::default();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, 1047, "synthetic(0 usage)/header 不计");
        let before = cur.consumed;
        read_update(f.path(), &mut cur); // 无新行
        assert_eq!(cur.consumed, before, "无新行 consumed 不变");
        // 追加一条真实行
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new().append(true).open(f.path()).unwrap();
        writeln!(fh, "{REAL_ASSISTANT}").unwrap(); fh.flush().unwrap();
        read_update(f.path(), &mut cur);
        assert_eq!(cur.consumed, before + 1047, "新行继续累加，单调");
    }

    #[test]
    fn discover_transcript_picks_newest_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(encode_cwd("/tmp/proj"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("old.jsonl"), "x").unwrap();
        let old_t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::File::open(proj.join("old.jsonl")).unwrap();
        f.set_modified(old_t).unwrap();
        std::fs::write(proj.join("new.jsonl"), "x").unwrap();
        std::fs::write(proj.join("ignore.txt"), "x").unwrap();
        let found = discover_transcript(dir.path(), "/tmp/proj").unwrap();
        assert!(found.ends_with("new.jsonl"));
        assert!(discover_transcript(dir.path(), "/no/such/cwd").is_none());
    }
}
