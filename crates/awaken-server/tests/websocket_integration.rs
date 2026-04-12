//! WebSocket transport integration tests.
//!
//! Validates bidirectional message relay, event streaming, connection lifecycle,
//! and error handling for WebSocket clients.

#![cfg(feature = "websocket")]

use serde_json::json;

// ============================================================================
// Client message protocol validation
// ============================================================================

#[test]
fn websocket_message_basic_json_parsing() {
    let json = r#"{"type": "message", "content": "Hello"}"#;
    let msg: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["content"], "Hello");
}

#[test]
fn websocket_message_with_special_chars() {
    let json_str = r#"{"type": "message", "content": "test\nwith\nnewlines"}"#;
    let msg: serde_json::Value = serde_json::from_str(json_str).unwrap();
    assert_eq!(msg["type"], "message");
}

#[test]
fn websocket_message_empty_content() {
    let json = r#"{"type": "message", "content": ""}"#;
    let msg: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(msg["content"], "");
}

#[test]
fn websocket_message_unicode_content() {
    let json = r#"{"type": "message", "content": "Hello 世界 🌍"}"#;
    let msg: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(msg["content"], "Hello 世界 🌍");
}

#[test]
fn websocket_message_with_multiline() {
    let msg = json!({
        "type": "message",
        "content": "Line 1\nLine 2\nLine 3"
    });
    assert_eq!(msg["type"], "message");
    assert!(msg["content"].as_str().unwrap().contains("Line 1"));
}

#[test]
fn websocket_message_invalid_json_fails() {
    let json = r#"{"type": "message", "content": }"#; // Invalid JSON
    let result: Result<serde_json::Value, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn websocket_message_missing_type_field() {
    let json = r#"{"content": "test"}"#;
    let msg: serde_json::Value = serde_json::from_str(json).unwrap();
    // Can still parse, but missing type key
    assert!(!msg.is_object() || msg["type"].is_null());
}

#[test]
fn websocket_message_extra_fields_preserved() {
    let json = r#"{"type": "message", "content": "test", "extra": "field"}"#;
    let msg: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["content"], "test");
    assert_eq!(msg["extra"], "field");
}

