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
//! | R7| `MemoryEntry` field set / value round-trip | pinned (Clone/Eq)                          |
//! | R7b| `MemoryEntry` serde round-trip         | wire mirrors a content-less `Memory`          |
//! | R8| `MAX_PATH_BYTES`                        | bare constant, no contract predicate (gap)    |

use awaken_resource_contract::{
    FileStoreError, MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, Memory, MemoryEntry,
    MemoryStoreError, SkillStoreError, validate_path_len,
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

// R7: `MemoryEntry` is the value `MemoryFs::list` returns for a directory listing.
// It carries the same identity/version/size fields as a content-less `Memory`. Its
// observable contract is its public field set plus value semantics (Clone, Eq), so
// a silent field add/rename/retype must break a test here — pin them.
//
// NOTE (see also the KNOWN GAP below): unlike `Memory`, `MemoryEntry` derives NO
// serde traits, so — by design of this test suite's "no src changes" rule — it
// cannot be pinned as a JSON wire shape the way `Memory` is above. Its round-trip
// here is therefore by value (Clone), not over the wire.
#[test]
fn memory_entry_value_round_trips_and_pins_field_shape() {
    let e = MemoryEntry {
        id: "mem_1".into(),
        path: "/notes/a.md".into(),
        content_sha256: "abc123".into(),
        content_size: 5,
        version: 1,
        updated_unix_nanos: 20,
    };

    // Clone is a lossless by-value round-trip; Eq is the equality contract adapters
    // rely on when diffing two listings (mirror/migration).
    let back = e.clone();
    assert_eq!(back, e, "MemoryEntry must be Clone + Eq (value semantics)");

    // Field-shape: pin every public field (name + type) so a schema drift trips here.
    assert_eq!(e.id, "mem_1");
    assert_eq!(e.path, "/notes/a.md");
    assert_eq!(e.content_sha256, "abc123");
    assert_eq!(e.content_size, 5u64);
    assert_eq!(e.version, 1u64);
    assert_eq!(e.updated_unix_nanos, 20u128);

    // Inequality must be observable on a single differing field — guards against a
    // future hand-rolled `PartialEq` that silently ignores one.
    let mut other = e.clone();
    other.content_sha256 = "different".into();
    assert_ne!(
        other, e,
        "differing content_sha256 must make entries unequal"
    );
}

// R7b: `MemoryEntry` derives `Serialize`/`Deserialize`, like `Memory` (lib.rs), so a
// `Vec<MemoryEntry>` from `MemoryFs::list` crosses the managed/HTTP surfaces directly
// (no re-projection through `Memory` or a bespoke adapter DTO). Its wire shape mirrors
// a content-less `Memory`: the same field names, no `content`. Pin that round-trip.
#[test]
fn memory_entry_round_trips_over_the_wire_mirroring_memory() {
    let e = MemoryEntry {
        id: "mem_2".into(),
        path: "/x".into(),
        content_sha256: "d".into(),
        content_size: 0,
        version: 3,
        updated_unix_nanos: 2,
    };
    let wire = serde_json::to_string(&e).unwrap();
    // The wire carries every identity/version/size field under the same names `Memory`
    // uses, and no `content` (a listing entry is content-less).
    for field in [
        "\"id\":\"mem_2\"",
        "\"path\":\"/x\"",
        "\"content_sha256\":\"d\"",
        "\"content_size\":0",
        "\"version\":3",
        "\"updated_unix_nanos\":2",
    ] {
        assert!(wire.contains(field), "wire must carry {field}: {wire}");
    }
    assert!(
        !wire.contains("content\":"),
        "a listing entry carries no content: {wire}"
    );
    assert_eq!(
        serde_json::from_str::<MemoryEntry>(&wire).unwrap(),
        e,
        "MemoryEntry round-trips over the wire"
    );
}

// R8: `MAX_PATH_BYTES` is a bare cap *constant*. Contrast `MAX_MEMORY_BYTES`, which
// the contract surfaces through a first-class port error (`MemErr::TooLarge`) that
// interpolates the cap — so every backend must reject over-cap content identically.
// The path cap now has a contract-level predicate too: `validate_path_len` maps an
// over-cap path to an `InvalidPath` that names `MAX_PATH_BYTES`, symmetric with
// `TooLarge`, so every backend enforces one identical limit instead of drifting.
#[test]
fn max_path_bytes_has_a_contract_level_enforcement_predicate() {
    // The cap is exposed as a constant...
    assert_eq!(MAX_PATH_BYTES, 1024);

    // ...and `validate_path_len` is the port's single path-length predicate: an
    // at-cap path is accepted, an over-cap path is rejected with an `InvalidPath`
    // that interpolates the cap — the same shape `TooLarge` gives for content.
    assert!(validate_path_len(&"x".repeat(MAX_PATH_BYTES)).is_ok());
    let err = validate_path_len(&"x".repeat(MAX_PATH_BYTES + 1)).unwrap_err();
    assert!(matches!(err, MemErr::InvalidPath(_)));
    let shown = err.to_string();
    assert!(shown.starts_with("invalid path: "));
    assert!(
        shown.contains(&MAX_PATH_BYTES.to_string()),
        "the predicate must name the path cap: {shown}"
    );
}
