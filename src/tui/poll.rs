//! poller 线程：严格串行 request → send → sleep（spec §9）。
//! 阻塞式请求天然串行不会并发累积；daemon 卡住时 client 的 100ms IO 超时
//! 保证单轮 ~150ms 内必返回（+daemon 不在时一次 50ms autospawn 重试）。

use crate::protocol::{Request, Response};
use crate::quota::QuotaState;
use crate::state::AgentSession;
use std::sync::mpsc::Sender;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub enum PollMsg {
    Snapshot {
        sessions: Vec<AgentSession>,
        quota: Vec<QuotaState>,
        generated_at_ms: u64,
    },
    Disconnected,
}

pub fn spawn(tx: Sender<PollMsg>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        let msg = to_msg(crate::client::request(&Request::Snapshot));
        if tx.send(msg).is_err() {
            return; // UI 已退出，receiver 没了
        }
        std::thread::sleep(POLL_INTERVAL);
    })
}

/// 纯函数：request 结果 → PollMsg（线程体只有 4 行，逻辑全在这，单测钉这里）。
fn to_msg(res: anyhow::Result<Response>) -> PollMsg {
    match res {
        Ok(Response::Snapshot { sessions, quota, generated_at_ms }) => {
            PollMsg::Snapshot { sessions, quota, generated_at_ms }
        }
        _ => PollMsg::Disconnected, // Err、Response::Ok/Error 一律视为断连
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_response_maps_to_snapshot_msg() {
        let msg = to_msg(Ok(Response::Snapshot { sessions: vec![], quota: vec![], generated_at_ms: 42 }));
        match msg {
            PollMsg::Snapshot { generated_at_ms, .. } => assert_eq!(generated_at_ms, 42),
            PollMsg::Disconnected => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn error_and_non_snapshot_map_to_disconnected() {
        assert!(matches!(to_msg(Err(anyhow::anyhow!("io"))), PollMsg::Disconnected));
        assert!(matches!(to_msg(Ok(Response::Ok)), PollMsg::Disconnected));
        assert!(matches!(
            to_msg(Ok(Response::Error { message: "x".into() })),
            PollMsg::Disconnected
        ));
    }
}
