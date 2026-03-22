//! Canonical progress and file activity types for tool call execution.

use serde::{Deserialize, Serialize};

/// Constants for activity type identification.
pub const TOOL_CALL_PROGRESS_ACTIVITY_TYPE: &str = "tool-call-progress";
pub const FILE_ACTIVITY_TYPE: &str = "file";

/// Canonical progress state for a tool call execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallProgressState {
    /// Schema identifier.
    #[serde(default = "default_schema")]
    pub schema: String,
    /// Unique node ID for this progress entry (typically tool_call_id).
    pub node_id: String,
    /// Tool call ID.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Current status.
    pub status: ProgressStatus,
    /// Normalized progress (0.0 - 1.0). None if indeterminate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Absolute progress loaded count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<u64>,
    /// Absolute progress total count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Human-readable status message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Parent node ID (for nested tool calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<String>,
    /// Parent tool call ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
}

fn default_schema() -> String {
    "tool-call-progress.v1".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// A file change event emitted during tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileActivity {
    /// File path (relative to workspace root).
    pub path: String,
    /// Type of change.
    pub operation: FileOperation,
    /// MIME type if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// File size in bytes after change. None for deletions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Created,
    Modified,
    Deleted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_state_serde_roundtrip() {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: "call-1".into(),
            call_id: "call-1".into(),
            tool_name: "search".into(),
            status: ProgressStatus::Running,
            progress: Some(0.5),
            loaded: Some(50),
            total: Some(100),
            message: Some("Searching...".into()),
            parent_node_id: None,
            parent_call_id: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.node_id, "call-1");
        assert_eq!(parsed.status, ProgressStatus::Running);
        assert_eq!(parsed.progress, Some(0.5));
        assert_eq!(parsed.loaded, Some(50));
        assert_eq!(parsed.total, Some(100));
        assert_eq!(parsed.message.as_deref(), Some("Searching..."));
    }

    #[test]
    fn progress_state_default_schema() {
        let json_str = r#"{
            "node_id": "n1",
            "call_id": "c1",
            "tool_name": "t1",
            "status": "pending"
        }"#;
        let parsed: ToolCallProgressState = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.schema, "tool-call-progress.v1");
    }

    #[test]
    fn progress_state_omits_none_fields() {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: "n1".into(),
            call_id: "c1".into(),
            tool_name: "t1".into(),
            status: ProgressStatus::Pending,
            progress: None,
            loaded: None,
            total: None,
            message: None,
            parent_node_id: None,
            parent_call_id: None,
        };
        let value: serde_json::Value = serde_json::to_value(&state).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("progress"));
        assert!(!obj.contains_key("loaded"));
        assert!(!obj.contains_key("total"));
        assert!(!obj.contains_key("message"));
        assert!(!obj.contains_key("parent_node_id"));
        assert!(!obj.contains_key("parent_call_id"));
    }

    #[test]
    fn progress_status_all_variants_roundtrip() {
        for status in [
            ProgressStatus::Pending,
            ProgressStatus::Running,
            ProgressStatus::Done,
            ProgressStatus::Failed,
            ProgressStatus::Cancelled,
        ] {
            let json = serde_json::to_value(status).unwrap();
            let parsed: ProgressStatus = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn progress_status_snake_case_serialization() {
        assert_eq!(
            serde_json::to_value(ProgressStatus::Pending).unwrap(),
            json!("pending")
        );
        assert_eq!(
            serde_json::to_value(ProgressStatus::Running).unwrap(),
            json!("running")
        );
        assert_eq!(
            serde_json::to_value(ProgressStatus::Done).unwrap(),
            json!("done")
        );
        assert_eq!(
            serde_json::to_value(ProgressStatus::Failed).unwrap(),
            json!("failed")
        );
        assert_eq!(
            serde_json::to_value(ProgressStatus::Cancelled).unwrap(),
            json!("cancelled")
        );
    }

    #[test]
    fn file_activity_serde_roundtrip() {
        let activity = FileActivity {
            path: "src/main.rs".into(),
            operation: FileOperation::Created,
            media_type: Some("text/x-rust".into()),
            size: Some(1024),
        };
        let json = serde_json::to_string(&activity).unwrap();
        let parsed: FileActivity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "src/main.rs");
        assert_eq!(parsed.operation, FileOperation::Created);
        assert_eq!(parsed.media_type.as_deref(), Some("text/x-rust"));
        assert_eq!(parsed.size, Some(1024));
    }

    #[test]
    fn file_activity_omits_none_fields() {
        let activity = FileActivity {
            path: "deleted.txt".into(),
            operation: FileOperation::Deleted,
            media_type: None,
            size: None,
        };
        let json = serde_json::to_string(&activity).unwrap();
        assert!(!json.contains("media_type"));
        assert!(!json.contains("size"));
    }

    #[test]
    fn file_operation_all_variants_roundtrip() {
        for op in [
            FileOperation::Created,
            FileOperation::Modified,
            FileOperation::Deleted,
        ] {
            let json = serde_json::to_value(op).unwrap();
            let parsed: FileOperation = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, op);
        }
    }

    #[test]
    fn file_operation_snake_case_serialization() {
        assert_eq!(
            serde_json::to_value(FileOperation::Created).unwrap(),
            json!("created")
        );
        assert_eq!(
            serde_json::to_value(FileOperation::Modified).unwrap(),
            json!("modified")
        );
        assert_eq!(
            serde_json::to_value(FileOperation::Deleted).unwrap(),
            json!("deleted")
        );
    }

    #[test]
    fn progress_state_with_parent_fields() {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: "child-1".into(),
            call_id: "child-1".into(),
            tool_name: "sub_tool".into(),
            status: ProgressStatus::Running,
            progress: None,
            loaded: None,
            total: None,
            message: None,
            parent_node_id: Some("parent-1".into()),
            parent_call_id: Some("parent-1".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("parent_node_id"));
        assert!(json.contains("parent_call_id"));
        let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.parent_node_id.as_deref(), Some("parent-1"));
        assert_eq!(parsed.parent_call_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn activity_type_constants() {
        assert_eq!(TOOL_CALL_PROGRESS_ACTIVITY_TYPE, "tool-call-progress");
        assert_eq!(FILE_ACTIVITY_TYPE, "file");
    }
}
