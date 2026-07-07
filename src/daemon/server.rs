use crate::event::AgentEvent;
use crate::protocol::{Request, Response};
use crate::state::StateStore;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub fn serve(listener: UnixListener, store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let store = Arc::clone(&store);
        let dirty = Arc::clone(&dirty);
        std::thread::spawn(move || handle(stream, store, dirty));
    }
}

fn handle(stream: UnixStream, store: Arc<Mutex<StateStore>>, dirty: Arc<AtomicBool>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() { continue; }
        let resp = respond(&line, &store, &dirty);
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() { return; }
    }
}

fn respond(line: &str, store: &Mutex<StateStore>, dirty: &AtomicBool) -> Response {
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
                    store.lock().unwrap().apply(ev);
                    dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
                None => Response::Error { message: format!("unknown event: {event}") },
            }
        }
        Request::Snapshot => {
            let sessions = store.lock().unwrap().sessions();
            Response::Snapshot { sessions, generated_at_ms: super::now_ms() }
        }
    }
}
