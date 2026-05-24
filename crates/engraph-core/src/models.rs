use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub feature: String,
    pub filter_id: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub latency_ms: i64,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBudget {
    pub session_id: String,
    pub soft_limit: i64,
    pub hard_limit: i64,
    pub used_tokens: i64,
    pub escalation_level: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum EventKind {
    Compress,
    Retrieve,
    Hook,
    WrappedCmd,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Compress => "compress",
            EventKind::Retrieve => "retrieve",
            EventKind::Hook => "hook",
            EventKind::WrappedCmd => "wrapped_cmd",
        }
    }
}
