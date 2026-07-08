use crate::protocol::{Request, Response};
use crate::{client, render};

pub fn run(format: &str) {
    let resp = client::request(&Request::Snapshot);
    match (format, resp) {
        ("json", Ok(Response::Snapshot { sessions, quota, .. })) => {
            let out = serde_json::json!({ "sessions": sessions, "quota": quota });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        ("tmux", Ok(Response::Snapshot { sessions, generated_at_ms, .. })) => {
            println!("{}", render::render_tmux(&sessions, generated_at_ms));
        }
        ("tmux", _) => println!("tfa:off"), // daemon 不可达
        (_, _) => println!("[]"),
    }
}
