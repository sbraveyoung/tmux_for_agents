use crate::client;
use crate::protocol::Request;
use std::io::Read;

/// hook 纪律：任何路径都 exit 0，绝不阻塞 agent。
pub fn run(agent: &str, event: &str) -> ! {
    let Ok(pane) = std::env::var("TMUX_PANE") else { std::process::exit(0) };
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let payload = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
    let _ = client::request(&Request::Hook {
        agent: agent.to_string(),
        event: event.to_string(),
        pane: Some(pane),
        payload,
    });
    std::process::exit(0)
}
