//! Machine, Session and Turn/Item entities (02 §1.1). These are stored as JSON documents in
//! `state.db` and crossed over the control API only; they never ride the postcard render
//! stream, so ordinary serde features are fine here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Local,
    Ssh,
    Quic,
}

impl MachineKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "local" => MachineKind::Local,
            "ssh" => MachineKind::Ssh,
            "quic" => MachineKind::Quic,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Connected,
    Connecting,
    Degraded,
    Offline,
}

impl MachineStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "connected" => MachineStatus::Connected,
            "connecting" => MachineStatus::Connecting,
            "degraded" => MachineStatus::Degraded,
            "offline" => MachineStatus::Offline,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            MachineStatus::Connected => "connected",
            MachineStatus::Connecting => "connecting",
            MachineStatus::Degraded => "degraded",
            MachineStatus::Offline => "offline",
        }
    }
}

/// A machine this session knows about (02 §1.1): this one, plus the remotes clients report.
/// `id` is the machine uuid for the local machine and a stable label-derived id for remotes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub label: String,
    pub kind: MachineKind,
    pub address: Option<String>,
    pub os: String,
    pub arch: String,
    pub vibeke_version: String,
    pub status: MachineStatus,
    pub last_seen_ms: i64,
}

/// The session as an API object (02 §1.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// `session_uuid` (changes only when the database is created).
    pub id: String,
    pub name: String,
    pub machine_id: String,
    pub created_at_ms: i64,
    pub server_pid: u32,
    pub server_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
}

/// One turn of a run (02 §1.1). `input_summary` is a redacted, bounded excerpt of the prompt,
/// never the full text (the tracking `turn` records hold exact prompts for tracked tasks only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub run_id: String,
    pub seq: u32,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub input_summary: String,
    pub status: TurnStatus,
    pub usage: Option<TurnUsage>,
    /// Session totals when the turn began; `usage` is the delta against them.
    pub usage_baseline: Option<TurnUsage>,
    pub item_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    ToolCall,
    ToolResult,
    FileChange,
    Command,
    Plan,
    Subagent,
    Error,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::UserMessage => "user_message",
            ItemKind::AssistantMessage => "assistant_message",
            ItemKind::Reasoning => "reasoning",
            ItemKind::ToolCall => "tool_call",
            ItemKind::ToolResult => "tool_result",
            ItemKind::FileChange => "file_change",
            ItemKind::Command => "command",
            ItemKind::Plan => "plan",
            ItemKind::Subagent => "subagent",
            ItemKind::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user_message" => ItemKind::UserMessage,
            "assistant_message" => ItemKind::AssistantMessage,
            "reasoning" => ItemKind::Reasoning,
            "tool_call" => ItemKind::ToolCall,
            "tool_result" => ItemKind::ToolResult,
            "file_change" => ItemKind::FileChange,
            "command" => ItemKind::Command,
            "plan" => ItemKind::Plan,
            "subagent" => ItemKind::Subagent,
            "error" => ItemKind::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Create,
    Modify,
    Delete,
    Rename,
}

impl FileOp {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create" => FileOp::Create,
            "modify" => FileOp::Modify,
            "delete" => FileOp::Delete,
            "rename" => FileOp::Rename,
            _ => return None,
        })
    }
}

/// `file_change` items carry this (02 §1.1): the collision tracker (05) and evidence bundles
/// read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub op: FileOp,
    pub lines_added: Option<u32>,
    pub lines_removed: Option<u32>,
}

/// One item of a turn (02 §1.1). Large payloads are in the blob store (`payload_ref` = blake3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub turn_id: String,
    pub run_id: String,
    pub seq: u32,
    pub kind: ItemKind,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub summary: String,
    pub payload_ref: Option<String>,
    pub file_change: Option<FileChange>,
    /// The harness's own id for the call (e.g. Claude `tool_use_id`), when it has one.
    pub native_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_wire_names() {
        for k in [
            ItemKind::UserMessage,
            ItemKind::AssistantMessage,
            ItemKind::Reasoning,
            ItemKind::ToolCall,
            ItemKind::ToolResult,
            ItemKind::FileChange,
            ItemKind::Command,
            ItemKind::Plan,
            ItemKind::Subagent,
            ItemKind::Error,
        ] {
            assert_eq!(ItemKind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
        assert_eq!(ItemKind::parse("nope"), None);
    }

    #[test]
    fn machine_enums_parse_and_serialize_snake_case() {
        assert_eq!(MachineKind::parse("quic"), Some(MachineKind::Quic));
        assert_eq!(
            MachineStatus::parse("degraded"),
            Some(MachineStatus::Degraded)
        );
        let m = Machine {
            id: "m1".into(),
            label: "devbox".into(),
            kind: MachineKind::Ssh,
            address: Some("me@devbox".into()),
            os: "linux".into(),
            arch: "x86_64".into(),
            vibeke_version: "0.1.0".into(),
            status: MachineStatus::Offline,
            last_seen_ms: 5,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "ssh");
        assert_eq!(v["status"], "offline");
        assert_eq!(serde_json::from_value::<Machine>(v).unwrap(), m);
    }
}
