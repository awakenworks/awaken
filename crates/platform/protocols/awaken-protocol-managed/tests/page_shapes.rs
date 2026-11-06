//! Every Managed list envelope and list query has the shape the official SDK
//! declares for it, field by field.
//!
//! The SDK declares pagination once, in `core/pagination.d.ts`, and every
//! Managed collection shares it. The generated oracle
//! (`contracts/anthropic-managed/upstream-oracle.generated.json`,
//! `current.wire_contract.pagination`) extracts the required and optional
//! properties of the three interfaces the Rust wire types mirror. The naming
//! check proves each Rust type cites one of those names; this test proves the
//! cited name is the right counterpart.
//!
//! Cause/effect graph: C1 a response envelope is serialized, including its
//! last or empty page; C2 the SDK declares required R and optional O for the
//! interface the envelope mirrors; C3 a list query is read from the request.
//! Effects: E1 every emitted key is in R ∪ O; E2 every key in R is emitted,
//! even when its value is null; E3 the query reads exactly the parameters the
//! SDK declares; E4 an SDK-shaped query string parses, and an absent optional
//! parameter reads as absent.
//!
//! | Rule | Rust type | SDK interface | Effect |
//! |---|---|---|---|
//! | G1 | `PageCursor<T>`, middle and last page | `PageCursorResponse` | E1 + E2 |
//! | G2 | `Page<T>`, full and empty page | `PageResponse` | E1 + E2 |
//! | G3 | `PageQuery` | `PageCursorParams` | E3 + E4 |
//!
//! E2 is the rule a client notices: `next_page` is `string | null`, required.
//! A last page that skipped the key would read as `undefined` and stop
//! pagination only by accident.

use std::collections::BTreeSet;

use awaken_protocol_managed::types::{Page, PageCursor, PageQuery};
use serde::Serialize;
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde_json::{Value, json};

fn pagination(interface: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../../../contracts/anthropic-managed/upstream-oracle.generated.json"
    ))
    .expect("generated Managed SDK oracle is valid JSON");
    let shape = &oracle["current"]["wire_contract"]["pagination"][interface];
    let names = |key: &str| -> BTreeSet<String> {
        shape[key]
            .as_array()
            .unwrap_or_else(|| panic!("the oracle extracts `{interface}.{key}`"))
            .iter()
            .map(|name| name.as_str().expect("a property name").to_string())
            .collect()
    };
    (names("required"), names("optional"))
}

fn emitted_keys(value: impl Serialize) -> BTreeSet<String> {
    serde_json::to_value(value)
        .expect("envelope serializes")
        .as_object()
        .expect("envelope is an object")
        .keys()
        .cloned()
        .collect()
}

/// E1 and E2 for one serialized envelope.
fn assert_envelope(rule: &str, interface: &str, keys: &BTreeSet<String>) {
    let (required, optional) = pagination(interface);
    for key in keys {
        assert!(
            required.contains(key) || optional.contains(key),
            "{rule}: emits `{key}`, which `{interface}` does not declare (E1)"
        );
    }
    for key in &required {
        assert!(
            keys.contains(key),
            "{rule}: omits required `{key}` of `{interface}` (E2)"
        );
    }
}

/// The field names a `Deserialize` struct reads, captured from serde's own
/// `deserialize_struct` call, so the test reads the Rust declaration itself
/// rather than a hand-kept copy of it.
fn declared_fields<T: DeserializeOwned>() -> BTreeSet<String> {
    struct Capture<'a>(&'a mut Option<&'static [&'static str]>);

    impl<'de> Deserializer<'de> for Capture<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = Some(fields);
            Err(de::Error::custom("fields captured"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }

    let mut fields = None;
    let _ = T::deserialize(Capture(&mut fields));
    fields
        .expect("the type deserializes as a struct")
        .iter()
        .map(|field| (*field).to_string())
        .collect()
}

#[test]
fn page_cursor_envelope_matches_page_cursor_response() {
    let middle = PageCursor {
        data: vec![json!({"id": "a"})],
        next_page: Some("cursor".into()),
    };
    assert_envelope("G1 middle", "PageCursorResponse", &emitted_keys(middle));
    let last = PageCursor::single(vec![json!({"id": "z"})]);
    let keys = emitted_keys(&last);
    assert_envelope("G1 last", "PageCursorResponse", &keys);
    assert_eq!(
        serde_json::to_value(&last).expect("serializes")["next_page"],
        Value::Null,
        "G1/E2 the last page states its end as null, not by omission"
    );
}

#[test]
fn id_page_envelope_matches_page_response() {
    let full = Page::new(
        vec![json!({"id": "a"}), json!({"id": "b"})],
        true,
        Some("a".into()),
        Some("b".into()),
    );
    assert_envelope("G2 full", "PageResponse", &emitted_keys(full));
    let empty = Page::<Value>::new(Vec::new(), false, None, None);
    assert_envelope("G2 empty", "PageResponse", &emitted_keys(&empty));
    let empty = serde_json::to_value(&empty).expect("serializes");
    assert_eq!(empty["first_id"], Value::Null, "G2/E2");
    assert_eq!(empty["last_id"], Value::Null, "G2/E2");
}

#[test]
fn page_query_reads_exactly_page_cursor_params() {
    let (required, optional) = pagination("PageCursorParams");
    let declared: BTreeSet<String> = required.union(&optional).cloned().collect();
    assert_eq!(
        declared_fields::<PageQuery>(),
        declared,
        "G3/E3 the query reads every SDK parameter and no other"
    );

    let full: PageQuery = serde_urlencoded::from_str("limit=5&page=cursor").expect("G3/E4");
    assert_eq!(
        full,
        PageQuery {
            limit: Some(5),
            page: Some("cursor".into()),
        },
        "G3/E4 an SDK-shaped query string parses"
    );
    let absent: PageQuery = serde_urlencoded::from_str("").expect("G3/E4");
    assert_eq!(
        absent,
        PageQuery::default(),
        "G3/E4 both parameters are optional"
    );
}
