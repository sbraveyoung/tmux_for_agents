use crate::protocol::{Request, Response};
use crate::{client, render};

pub fn run(format: &str) {
    let resp = client::request(&Request::Snapshot);
    match (format, resp) {
        ("json", Ok(Response::Snapshot { sessions, quota, .. })) => {
            let out = serde_json::json!({ "sessions": sessions, "quota": quota });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        ("tmux", Ok(Response::Snapshot { sessions, quota, generated_at_ms, .. })) => {
            // [quota] status_bar_percent 开启且快照里有一条 RealApi 来源的 Claude
            // 配额时才传 Some——默认关闭，LocalEstimate 从不假造百分比（2026-07-14）。
            let cfg = crate::config::Config::load();
            let pct = cfg.quota.status_bar_percent.then(|| {
                quota.iter()
                    .find(|q| matches!(q.source, crate::quota::QuotaSource::RealApi)
                        && q.provider == crate::event::AgentKind::Claude)
                    .and_then(|q| q.window_5h_percent)
            }).flatten();
            println!("{}", render::render_tmux(&sessions, generated_at_ms, pct));
        }
        ("tmux", _) => println!("tfa:off"), // daemon 不可达
        (_, _) => println!("[]"),
    }
}
