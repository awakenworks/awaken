//! Canonical progress and file activity types for tool call execution.

use serde::{Deserialize, Serialize};

/// Constants for activity type identification.
pub const TOOL_CALL_PROGRESS_ACTIVITY_TYPE: &str = "tool-call-progress";

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
    /// Run ID of the owning agent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Parent run ID (set when this run was spawned by another run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Thread ID of the owning thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
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
            run_id: None,
            parent_run_id: None,
            thread_id: None,
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
            run_id: None,
            parent_run_id: None,
            thread_id: None,
        };
        let value: serde_json::Value = serde_json::to_value(&state).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("progress"));
        assert!(!obj.contains_key("loaded"));
        assert!(!obj.contains_key("total"));
        assert!(!obj.contains_key("message"));
        assert!(!obj.contains_key("parent_node_id"));
        assert!(!obj.contains_key("parent_call_id"));
        assert!(!obj.contains_key("run_id"));
        assert!(!obj.contains_key("parent_run_id"));
        assert!(!obj.contains_key("thread_id"));
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
            run_id: None,
            parent_run_id: None,
            thread_id: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("parent_node_id"));
        assert!(json.contains("parent_call_id"));
        let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.parent_node_id.as_deref(), Some("parent-1"));
        assert_eq!(parsed.parent_call_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn progress_state_lineage_fields_roundtrip() {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: "tool_call:call-42".into(),
            call_id: "call-42".into(),
            tool_name: "search".into(),
            status: ProgressStatus::Running,
            progress: None,
            loaded: None,
            total: None,
            message: None,
            parent_node_id: Some("run:run-1".into()),
            parent_call_id: None,
            run_id: Some("run-1".into()),
            parent_run_id: Some("run-0".into()),
            thread_id: Some("thread-abc".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("run_id"));
        assert!(json.contains("parent_run_id"));
        assert!(json.contains("thread_id"));
        let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id.as_deref(), Some("run-1"));
        assert_eq!(parsed.parent_run_id.as_deref(), Some("run-0"));
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn activity_type_constants() {
        assert_eq!(TOOL_CALL_PROGRESS_ACTIVITY_TYPE, "tool-call-progress");
    }

    #[test]
    fn progress_status_all_variants_have_distinct_serialization() {
        use std::collections::HashSet;

        let variants = [
            ProgressStatus::Pending,
            ProgressStatus::Running,
            ProgressStatus::Done,
            ProgressStatus::Failed,
            ProgressStatus::Cancelled,
        ];
        let mut seen = HashSet::new();
        for variant in &variants {
            let serialized = serde_json::to_string(variant).unwrap();
            assert!(
                seen.insert(serialized.clone()),
                "Duplicate serialization: {serialized} for {variant:?}"
            );
            let parsed: ProgressStatus = serde_json::from_str(&serialized).unwrap();
            assert_eq!(&parsed, variant, "Roundtrip failed for {variant:?}");
        }
        assert_eq!(seen.len(), 5, "Expected 5 distinct serialized strings");
    }

    #[test]
    fn progress_state_with_all_fields_populated() {
        let state = ToolCallProgressState {
            schema: "tool-call-progress.v1".into(),
            node_id: "tool_call:call-99".into(),
            call_id: "call-99".into(),
            tool_name: "complex_tool".into(),
            status: ProgressStatus::Running,
            progress: Some(0.75),
            loaded: Some(750),
            total: Some(1000),
            message: Some("Processing batch 3 of 4".into()),
            parent_node_id: Some("run:parent-run".into()),
            parent_call_id: Some("parent-call-1".into()),
            run_id: Some("run-42".into()),
            parent_run_id: Some("run-41".into()),
            thread_id: Some("thread-xyz".into()),
        };

        let value: serde_json::Value = serde_json::to_value(&state).unwrap();
        let obj = value.as_object().unwrap();

        // Verify all fields are present in serialized output.
        let expected_keys = [
            "schema",
            "node_id",
            "call_id",
            "tool_name",
            "status",
            "progress",
            "loaded",
            "total",
            "message",
            "parent_node_id",
            "parent_call_id",
            "run_id",
            "parent_run_id",
            "thread_id",
        ];
        for key in &expected_keys {
            assert!(obj.contains_key(*key), "Missing key: {key}");
        }

        // Roundtrip check.
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema, "tool-call-progress.v1");
        assert_eq!(parsed.node_id, "tool_call:call-99");
        assert_eq!(parsed.call_id, "call-99");
        assert_eq!(parsed.tool_name, "complex_tool");
        assert_eq!(parsed.status, ProgressStatus::Running);
        assert_eq!(parsed.progress, Some(0.75));
        assert_eq!(parsed.loaded, Some(750));
        assert_eq!(parsed.total, Some(1000));
        assert_eq!(parsed.message.as_deref(), Some("Processing batch 3 of 4"));
        assert_eq!(parsed.parent_node_id.as_deref(), Some("run:parent-run"));
        assert_eq!(parsed.parent_call_id.as_deref(), Some("parent-call-1"));
        assert_eq!(parsed.run_id.as_deref(), Some("run-42"));
        assert_eq!(parsed.parent_run_id.as_deref(), Some("run-41"));
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-xyz"));
    }

    #[test]
    fn progress_state_validates_progress_range() {
        // The progress field has no built-in validation; this test documents
        // that arbitrary finite f64 values serialize and deserialize without error.
        let finite_cases: &[f64] = &[0.0, 0.5, 1.0, -1.0, 2.0];
        for &val in finite_cases {
            let state = ToolCallProgressState {
                schema: "tool-call-progress.v1".into(),
                node_id: "n".into(),
                call_id: "c".into(),
                tool_name: "t".into(),
                status: ProgressStatus::Pending,
                progress: Some(val),
                loaded: None,
                total: None,
                message: None,
                parent_node_id: None,
                parent_call_id: None,
                run_id: None,
                parent_run_id: None,
                thread_id: None,
            };
            let json = serde_json::to_string(&state).unwrap();
            let parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.progress,
                Some(val),
                "Roundtrip failed for progress={val}"
            );
        }

        // Non-finite values: serde_json may reject or serialize them depending
        // on the version. Document observed behavior for each.
        for &non_finite in &[f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let state = ToolCallProgressState {
                schema: "tool-call-progress.v1".into(),
                node_id: "n".into(),
                call_id: "c".into(),
                tool_name: "t".into(),
                status: ProgressStatus::Pending,
                progress: Some(non_finite),
                loaded: None,
                total: None,
                message: None,
                parent_node_id: None,
                parent_call_id: None,
                run_id: None,
                parent_run_id: None,
                thread_id: None,
            };
            match serde_json::to_string(&state) {
                Ok(json) => {
                    // If serialization succeeds, verify it can be parsed back.
                    // The value may lose fidelity (e.g. null for NaN).
                    let _parsed: ToolCallProgressState = serde_json::from_str(&json).unwrap();
                }
                Err(_) => {
                    // serde_json correctly rejects non-finite float.
                }
            }
        }
    }
}
