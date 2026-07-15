//! Cause-effect coverage for the port-only resource contract.
//!
//! A contract crate has no backend behaviour to exercise; its *observable*
//! contract is (a) the serde wire shape of [`Memory`] — which protocols
//! serialize onto the managed/HTTP surfaces — and (b) the `Display` messages of
//! the error enums, which those same surfaces render into RFC-9457 problem
//! details. Both are load-bearing across adapters, so they are pinned here.
//!
//! Decision table (causes → effects):
//!
//! | # | Cause                                   | Effect                                        |
//! |---|-----------------------------------------|-----------------------------------------------|
//! | R1| `Memory.content = Some(x)`              | serialized JSON carries a `"content"` key     |
//! | R2| `Memory.content = None` (a listing)     | `"content"` key omitted (skip_serializing_if) |
//! | R3| JSON without `content` (a listing wire) | deserializes to `content = None` (serde default)|
//! | R4| `MemErr::TooLarge`                      | message interpolates `MAX_MEMORY_BYTES`       |
//! | R5| `MemErr::Conflict { current }`          | message embeds `current.path`                 |
//! | R6| each error variant                      | stable `Display` prefix (adapter-facing)      |

use awaken_resource_contract::{
    FileStoreError, MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, Memory, MemoryStoreError,
    SkillStoreError,
};

fn sample_memory(content: Option<&str>) -> Memory {
    Memory {
        id: "mem_1".into(),
        path: "/notes/a.md".into(),
        content_sha256: "abc123".into(),
        content_size: 5,
        version: 1,
        created_unix_nanos: 10,
        updated_unix_nanos: 20,
        content: content.map(|s| s.to_string()),
    }
}

// R1: content present → key present, and full round-trip is lossless.
#[test]
fn memory_with_content_serializes_the_key_and_round_trips() {
    let m = sample_memory(Some("hello"));
    let v: serde_json::Value = serde_json::to_value(&m).unwrap();
    assert_eq!(v.get("content").and_then(|c| c.as_str()), Some("hello"));

    let back: Memory = serde_json::from_value(v).unwrap();
    assert_eq!(back, m, "get-shaped Memory must round-trip losslessly");
}

// R2: content absent (a directory listing) → key omitted entirely, not `null`.
#[test]
fn memory_without_content_omits_the_key() {
    let m = sample_memory(None);
    let v: serde_json::Value = serde_json::to_value(&m).unwrap();
    assert!(
        v.get("content").is_none(),
        "listing Memory must omit `content`, not emit null: {v}"
    );
}

// R3: a listing wire (no `content`) deserializes back with content = None.
#[test]
fn memory_wire_without_content_defaults_to_none() {
    let wire = serde_json::json!({
        "id": "mem_2",
        "path": "/x",
        "content_sha256": "d",
        "content_size": 0,
        "version": 3,
        "created_unix_nanos": 1u128,
        "updated_unix_nanos": 2u128,
    });
    let m: Memory = serde_json::from_value(wire).unwrap();
    assert_eq!(m.content, None);
    assert_eq!(m.version, 3);
}

// R4: the size-cap error surfaces the exact byte cap so callers/UIs can echo it.
#[test]
fn too_large_display_interpolates_the_cap() {
    let msg = MemErr::TooLarge.to_string();
    assert_eq!(msg, format!("content exceeds {MAX_MEMORY_BYTES} bytes"));
    assert!(msg.contains("102400"));
}

// R5: a CAS conflict must carry (and render) the live path so a client can rebase.
#[test]
fn conflict_display_embeds_current_path() {
    let err = MemErr::Conflict {
        current: Box::new(sample_memory(Some("live"))),
    };
    assert_eq!(
        err.to_string(),
        "cas conflict on /notes/a.md: base sha stale"
    );
}

// R6: every variant's Display prefix is part of the adapter contract.
#[test]
fn error_display_prefixes_are_stable() {
    assert_eq!(
        MemErr::NotFound("k".into()).to_string(),
        "memory not found: k"
    );
    assert_eq!(
        MemErr::PathConflict("/p".into()).to_string(),
        "path already exists: /p"
    );
    assert_eq!(
        MemErr::InvalidPath("..".into()).to_string(),
        "invalid path: .."
    );
    assert_eq!(MemErr::Storage("disk".into()).to_string(), "storage: disk");

    assert_eq!(
        FileStoreError("boom".into()).to_string(),
        "file store error: boom"
    );
    assert_eq!(SkillStoreError::Io("io".into()).to_string(), "io: io");
    assert_eq!(
        SkillStoreError::Storage("s".into()).to_string(),
        "storage: s"
    );
    assert_eq!(MemoryStoreError::Io("io".into()).to_string(), "io: io");
    assert_eq!(
        MemoryStoreError::Storage("s".into()).to_string(),
        "storage: s"
    );
}

// The two hard caps are part of the wire contract (Anthropic parity, ADR-0057);
// a silent change here is a cross-adapter break, so pin the literals.
#[test]
fn hard_caps_are_pinned() {
    assert_eq!(MAX_MEMORY_BYTES, 102_400);
    assert_eq!(MAX_PATH_BYTES, 1024);
}
