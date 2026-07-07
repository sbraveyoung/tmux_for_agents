//! Scanner reconcile loop: periodically cross-checks tmux panes + processes
//! against the StateStore. This is what closes the "sidebar blind spot" —
//! sessions the hook channel never saw (daemon started late, hooks not
//! wired for an agent, etc.) — and what corrects liveness when a pane
//! disappears without ever firing a SessionEnd hook (the M1 "ghost
//! working" failure mode).

pub mod procs;

use crate::sources::claude_jsonl::{self, TranscriptCursor};
use crate::state::StateStore;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
pub fn spawn(store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    if std::env::var("TFA_NO_SCAN").as_deref() == Ok("1") {
        return;
    }
    std::thread::spawn(move || {
        let mut cursors: HashMap<String, TranscriptCursor> = HashMap::new();
        let mut cursor_paths: HashMap<String, PathBuf> = HashMap::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(scan_interval_ms()));
            if tick(&store, &mut cursors, &mut cursor_paths, crate::daemon::now_ms()) {
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
/// 5. `session_name` backfill, reusing this round's pane table (no extra
///    tmux call).
/// 6. Claude transcript metrics: discover + bind a transcript for panes that
///    don't have one yet, then read incremental updates for the rest.
///
/// Returns whether the round produced any (potential) change, so the caller
/// can flag the shared `dirty` bit for the snapshot writer.
pub fn tick(
    store: &Mutex<StateStore>,
    cursors: &mut HashMap<String, TranscriptCursor>,
    cursor_paths: &mut HashMap<String, PathBuf>,
    now_ms: u64,
) -> bool {
    let panes = procs::list_panes();
    if panes.is_empty() && !crate::daemon::lifecycle::tmux_alive() {
        // Can't tell "tmux is down" apart from "tmux has no panes" from
        // list_panes' empty Vec alone — skip this round to avoid falsely
        // marking every live session Dead.
        return false;
    }
    let procs_list = procs::list_procs();

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
        st.upsert_scanned(&pane.pane_id, kind.clone(), *pid, &pane.cwd, &pane.session_name, now_ms);
    }
    st.reconcile_liveness(&live, now_ms);
    st.stale_sweep(now_ms);
    // session_name backfill, reusing this round's pane table.
    let by_pane: HashMap<&str, &str> =
        panes.iter().map(|p| (p.pane_id.as_str(), p.session_name.as_str())).collect();
    for pane_id in st.panes_needing_name() {
        if let Some(name) = by_pane.get(pane_id.as_str()) {
            st.set_session_name(&pane_id, name.to_string());
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
        if let Some(m) = claude_jsonl::read_update(&path, cursor) {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_metrics(&pane_id, m);
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
                    st.set_metrics(&pane_id, m);
                }
            }
        }
    }
    true
}
