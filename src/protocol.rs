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
        sessions: Vec<crate::state::AgentSession>,
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

    #[test]
    fn snapshot_wire_shape() {
        let snapshot = Response::Snapshot {
            sessions: vec![],
            generated_at_ms: 42,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains(r#""result":"snapshot""#));
        assert!(json.contains(r#""generated_at_ms":42"#));

        let error = Response::Error { message: "x".into() };
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains(r#""result":"error""#));
    }
}
