use crate::config::Config;
use crate::event::AgentEvent;
use crate::notify::discipline::Discipline;
use crate::notify::NotifyEvent;
use crate::protocol::{Request, Response};
use crate::quota::QuotaCache;
use crate::state::StateStore;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[allow(clippy::too_many_arguments)]
pub fn serve(
    listener: UnixListener,
    store: Arc<Mutex<StateStore>>,
    dirty: Arc<AtomicBool>,
    quota: Arc<Mutex<QuotaCache>>,
    config: Arc<Mutex<Config>>,
    discipline: Arc<Mutex<Discipline>>,
    tx: Sender<NotifyEvent>,
) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let store = Arc::clone(&store);
        let dirty = Arc::clone(&dirty);
        let quota = Arc::clone(&quota);
        let config = Arc::clone(&config);
        let discipline = Arc::clone(&discipline);
        let tx = tx.clone();
        std::thread::spawn(move || handle(stream, store, dirty, quota, config, discipline, tx));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle(
    stream: UnixStream,
    store: Arc<Mutex<StateStore>>,
    dirty: Arc<AtomicBool>,
    quota: Arc<Mutex<QuotaCache>>,
    config: Arc<Mutex<Config>>,
    discipline: Arc<Mutex<Discipline>>,
    tx: Sender<NotifyEvent>,
) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() { continue; }
        let resp = respond(&line, &store, &dirty, &quota, &config, &discipline, &tx);
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() { return; }
    }
}

#[allow(clippy::too_many_arguments)]
fn respond(
    line: &str,
    store: &Mutex<StateStore>,
    dirty: &AtomicBool,
    quota: &Mutex<QuotaCache>,
    config: &Mutex<Config>,
    discipline: &Mutex<Discipline>,
    tx: &Sender<NotifyEvent>,
) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Response::Error { message: format!("bad request: {e}") },
    };
    match req {
        Request::Hook { agent, event, pane, payload } => {
            let Some(pane) = pane else {
                return Response::Error { message: "missing pane".into() };
            };
            match AgentEvent::from_hook(&agent, &event, &pane, &payload, super::now_ms()) {
                Some(ev) => {
                    let pane = ev.pane_id.clone();
                    // 净边沿计算在锁内完成（before 快照 → apply → after 快照 → edges）；
                    // tx.send 在锁外（护栏：hook 路径的通道 IO 只归 notifier 线程，respond()
                    // 这里只负责在锁内收集 NotifyEvent，出锁后非阻塞入队）。
                    let (needs_name, evs) = {
                        let mut st = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let before = Discipline::snapshot_states(&st.sessions());
                        st.apply(ev);
                        let after = st.sessions();
                        let needs_name = after.iter()
                            .find(|s| s.pane_id == pane)
                            .is_some_and(|s| s.session_name.is_none());
                        let cfg = config.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                        let evs = discipline.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            .edges(&before, &after, &cfg.notify, super::now_ms());
                        (needs_name, evs)
                    };
                    if needs_name {
                        if let Some(name) = resolve_session_name(&pane) {
                            store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_session_name(&pane, name);
                        }
                    }
                    for ev in evs { let _ = tx.send(ev); }
                    dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
                None => Response::Error { message: format!("unknown event: {event}") },
            }
        }
        Request::Snapshot => {
            let sessions = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions();
            let quota = quota.lock().unwrap_or_else(std::sync::PoisonError::into_inner).states();
            Response::Snapshot { sessions, quota, generated_at_ms: super::now_ms() }
        }
    }
}

fn resolve_session_name(pane_id: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(crate::paths::tmux_args())
        .args(["display-message", "-p", "-t", pane_id, "#{session_name}"]);
    let out = cmd.output().ok()?;
    if !out.status.success() { return None; }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}
