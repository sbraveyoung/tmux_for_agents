use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hook {
        agent: String,
        event: String,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        payload: serde_json::Value,
    },
    Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Snapshot {
        sessions: Vec<serde_json::Value>,
        generated_at_ms: u64,
    },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let line = r#"{"op":"hook","agent":"claude","event":"stop","pane":"%3","payload":{}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert!(matches!(req, Request::Hook { .. }));
        let back = serde_json::to_string(&req).unwrap();
        assert!(back.contains(r#""op":"hook""#));
    }
}
