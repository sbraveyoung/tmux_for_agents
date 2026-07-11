//! Scanner reconcile loop: periodically cross-checks tmux panes + processes
//! against the StateStore. This is what closes the "sidebar blind spot" —
//! sessions the hook channel never saw (daemon started late, hooks not
//! wired for an agent, etc.) — and what corrects liveness when a pane
//! disappears without ever firing a SessionEnd hook (the M1 "ghost
//! working" failure mode).

pub mod procs;

use crate::config::Config;
use crate::notify::discipline::Discipline;
use crate::notify::NotifyEvent;
use crate::quota::burn::BurnSampler;
use crate::quota::QuotaCache;
use crate::sources::claude_jsonl::{self, TranscriptCursor};
use crate::state::StateStore;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

fn scan_interval_ms() -> u64 {
    std::env::var("TFA_SCAN_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15_000)
}

/// Starts the background scanner thread. A no-op when `TFA_NO_SCAN=1` —
/// existing daemon tests written against M1-only behavior opt out this way
/// rather than tolerate scanner side effects they don't expect.
pub fn spawn(
    store: Arc<Mutex<StateStore>>,
    dirty: Arc<AtomicBool>,
    quota: Arc<Mutex<QuotaCache>>,
    config: Arc<Mutex<Config>>,
    discipline: Arc<Mutex<Discipline>>,
    tx: Sender<NotifyEvent>,
) {
    if std::env::var("TFA_NO_SCAN").as_deref() == Ok("1") {
        return;
    }
    std::thread::spawn(move || {
        let mut cursors: HashMap<String, TranscriptCursor> = HashMap::new();
        let mut cursor_paths: HashMap<String, PathBuf> = HashMap::new();
        let mut burn = BurnSampler::new(crate::config::Config::load().quota.burn_rate_window_mins);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(scan_interval_ms()));
            if tick(&store, &mut cursors, &mut cursor_paths, crate::daemon::now_ms(), &mut burn, &quota, &config, &discipline, &tx) {
                dirty.store(true, Ordering::Relaxed);
            }
        }
    });
}

/// Runs a single reconcile round. Order is the spec:
/// 1. `list_panes` — tmux being unreachable and tmux having zero panes are
///    indistinguishable from the command output alone, so when the pane
///    list comes back empty we consult `lifecycle::tmux_alive()`; if tmux
///    really is gone we skip the round entirely rather than mass-mark every
///    session Dead.
/// 2. `list_procs` + `find_agent` per pane → `upsert_scanned` for hits.
/// 3. Build the full `pane_id -> Option<pid>` liveness map → `reconcile_liveness`.
/// 4. `stale_sweep`.
/// 5. `session_name` + 坐标 (window/pane index) backfill, reusing this
///    round's pane table (no extra tmux call).
/// 6. Claude transcript metrics: discover + bind a transcript for panes that
///    don't have one yet, then read incremental updates for the rest.
///
/// Returns whether the round produced any (potential) change, so the caller
/// can flag the shared `dirty` bit for the snapshot writer.
#[allow(clippy::too_many_arguments)]
pub fn tick(
    store: &Mutex<StateStore>,
    cursors: &mut HashMap<String, TranscriptCursor>,
    cursor_paths: &mut HashMap<String, PathBuf>,
    now_ms: u64,
    burn: &mut BurnSampler,
    quota: &Mutex<QuotaCache>,
    config: &Mutex<Config>,
    discipline: &Mutex<Discipline>,
    tx: &Sender<NotifyEvent>,
) -> bool {
    // tick 开头：在本轮任何 mutation 之前拍 before 快照（通知边沿 diff 的基线）。
    let before = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Discipline::snapshot_states(&st.sessions())
    };
    // list_panes→lock TOCTOU: a pane can get hooked between this snapshot and
    // the state-store lock below, so a brand-new hook-reported session can be
    // absent from `live` and get marked Dead for (at most) this one round;
    // the next Activity hook or scan tick self-heals it via the Dead-revive
    // path in reconcile_liveness. Accepted — narrower and cheaper than
    // holding the lock across the tmux/ps subprocess calls.
    let Some(panes) = procs::list_panes() else {
        // tmux command itself failed (not just "zero panes") — treat as
        // transient and skip the round rather than mass-mark every session
        // Dead off a single flaky invocation (F1).
        return false;
    };
    if panes.is_empty() && !crate::daemon::lifecycle::tmux_alive() {
        // Can't tell "tmux is down" apart from "tmux has no panes" from
        // list_panes' empty Vec alone — skip this round to avoid falsely
        // marking every live session Dead.
        return false;
    }
    let procs_list = procs::list_procs();
    if ps_failed(panes.is_empty(), procs_list.is_empty()) {
        // panes 非空但 procs 为空 = ps 失败（活系统不可能无进程）→ 跳过本轮防误杀
        return false;
    }

    let mut live: BTreeMap<String, Option<u32>> = BTreeMap::new();
    let mut agent_hits: Vec<(procs::PaneInfo, crate::event::AgentKind, u32)> = Vec::new();
    for pane in &panes {
        match procs::find_agent(pane.pane_pid, &procs_list) {
            Some((kind, pid)) => {
                live.insert(pane.pane_id.clone(), Some(pid));
                agent_hits.push((pane.clone(), kind, pid));
            }
            None => {
                live.insert(pane.pane_id.clone(), None);
            }
        }
    }

    // —— state reconcile (single lock section) ——
    let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (pane, kind, pid) in &agent_hits {
        st.upsert_scanned(&pane.pane_id, kind.clone(), *pid, pane.window_index, pane.pane_index, &pane.cwd, &pane.session_name, now_ms);
    }
    st.reconcile_liveness(&live, now_ms);
    st.stale_sweep(now_ms);
    // session_name + 坐标(window/pane index) backfill, reusing this round's pane
    // table (no extra tmux call).
    let by_pane: HashMap<&str, (&str, u32, u32)> = panes
        .iter()
        .map(|p| (p.pane_id.as_str(), (p.session_name.as_str(), p.window_index, p.pane_index)))
        .collect();
    for pane_id in st.panes_needing_name() {
        if let Some((name, _, _)) = by_pane.get(pane_id.as_str()) {
            st.set_session_name(&pane_id, name.to_string());
        }
    }
    // 坐标：覆盖本轮 pane 表里出现的每一条会话（含 hook-only、本轮未命中 agent
    // 因而没走 upsert_scanned 的），每轮都跑一遍以便 move-pane/move-window 后自愈。
    for s in st.sessions() {
        if let Some((_, w, p)) = by_pane.get(s.pane_id.as_str()) {
            st.set_location(&s.pane_id, *w, *p);
        }
    }
    // —— claude transcript metrics ——
    let claude_targets: Vec<(String, Option<String>, Option<String>)> = st
        .sessions()
        .iter()
        .filter(|s| {
            matches!(s.agent, crate::event::AgentKind::Claude)
                && !matches!(s.state, crate::state::SessionState::Dead)
        })
        .map(|s| (s.pane_id.clone(), s.transcript_path.clone(), s.cwd.clone()))
        .collect();
    drop(st);

    for (pane_id, transcript, cwd) in claude_targets {
        let path = match transcript {
            Some(t) => {
                let p = PathBuf::from(t);
                // The bound transcript changed under us (e.g. a new session's
                // file) — the cursor we're holding has an offset that
                // belongs to a different file, so drop it.
                if cursor_paths.get(&pane_id) != Some(&p) {
                    cursors.remove(&pane_id);
                }
                p
            }
            None => {
                // transcript_path was reset (SessionStart) or never known —
                // any cursor we're holding is stale.
                cursors.remove(&pane_id);
                cursor_paths.remove(&pane_id);
                let Some(cwd) = cwd.as_deref() else { continue };
                let Some(found) = claude_jsonl::discover_transcript(&crate::paths::projects_dir(), cwd) else {
                    continue;
                };
                store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_transcript(&pane_id, found.to_string_lossy().into_owned());
                found
            }
        };
        cursor_paths.insert(pane_id.clone(), path.clone());
        let cursor = cursors.entry(pane_id.clone()).or_default();
        // trust domain note: `path` is a transcript_path that ultimately came
        // from a hook payload (or discover_transcript's own directory walk).
        // Any process that can write to the daemon's socket already has to
        // be the same uid as the daemon (0700 socket dir) to connect at all,
        // so treating the payload's path as trusted here doesn't widen the
        // attack surface. Reads are bounded regardless: read_update caps the
        // first read at TAIL_CAP and is incremental after, so a pathological
        // (huge/binary/malformed) file yields None rather than unbounded work.
        let m = claude_jsonl::read_update(&path, cursor);
        let consumed_now = cursor.consumed;
        {
            let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            st.set_consumed(&pane_id, consumed_now);
            if let Some(m) = m { st.set_metrics(&pane_id, m, now_ms); }
        }
    }
    // —— codex 指标 ——
    let codex_targets: Vec<(String, Option<String>)> = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.sessions().iter()
            .filter(|s| matches!(s.agent, crate::event::AgentKind::Codex)
                && !matches!(s.state, crate::state::SessionState::Dead))
            .map(|s| (s.pane_id.clone(), s.cwd.clone()))
            .collect()
    };
    if !codex_targets.is_empty() {
        let threads = crate::sources::codex_db::load_recent(200);
        if !threads.is_empty() {
            let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (pane_id, cwd) in codex_targets {
                let Some(cwd) = cwd.as_deref() else { continue };
                if let Some(m) = crate::sources::codex_db::metrics_for(&threads, cwd) {
                    // codex tokens_used 即 consumed 口径（per-thread 累计）
                    if let Some(t) = threads.iter().filter(|t| t.cwd == cwd).max_by_key(|t| t.updated_at_ms) {
                        st.set_consumed(&pane_id, t.tokens_used);
                    }
                    st.set_metrics(&pane_id, m, now_ms);
                }
            }
        }
    }

    // —— burn 采样 + quota 刷新 ——
    let samples: Vec<(String, crate::event::AgentKind, u64)> = {
        let st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.sessions().iter().map(|s| (s.stable_key(), s.agent.clone(), s.consumed_tokens)).collect()
    };
    burn.sample(&samples, now_ms);
    quota.lock().unwrap_or_else(std::sync::PoisonError::into_inner).refresh(burn, now_ms);

    // —— 通知纪律：tick 边界快照 diff → 净边沿 → mpsc 入队（锁外发送）——
    let after = { store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions() };
    let cfg = { config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() };
    let evs = { discipline.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edges(&before, &after, &cfg.notify, now_ms) };
    for ev in evs { let _ = tx.send(ev); }

    true
}

/// panes 非空但 procs 为空 = ps 失败（活系统不可能无进程）→ 跳过本轮防误杀
fn ps_failed(panes_empty: bool, procs_empty: bool) -> bool {
    !panes_empty && procs_empty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_failure_guard_table() {
        assert!(ps_failed(false, true));   // panes 在、procs 空 → ps 挂了
        assert!(!ps_failed(false, false)); // 正常
        assert!(!ps_failed(true, true));   // 无 pane：交给 tmux_alive 分支
        assert!(!ps_failed(true, false));
    }
}
