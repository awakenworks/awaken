use super::*;

#[tokio::test]
async fn original_body_primitive_bounds_complete_fresh_data_without_legacy_truncation() {
    // R3/R4/R9/V4: complete raw body B-1/B/B+1 is accepted/accepted/error;
    // no truncation or Content-Length assumption. Explicit legacy body
    // keeps its old complete bytes; only exact known markers select that
    // exception. Unknown markers are not a compatibility fallback.
    for length in [RESPONSE_BYTES - 1, RESPONSE_BYTES, RESPONSE_BYTES + 1] {
        let result = collect_create_body(
            Body::from(vec![b'x'; length]),
            CreateReplyProfile::FreshCreate,
        )
        .await;
        assert_eq!(result.is_ok(), length <= RESPONSE_BYTES);
        if let Ok(bytes) = result {
            assert_eq!(bytes.len(), length);
        }
    }
    let legacy = collect_create_body(
        Body::from(vec![b'x'; RESPONSE_BYTES + 1]),
        CreateReplyProfile::LegacyRetainedCreate,
    )
    .await
    .unwrap();
    assert_eq!(legacy.len(), RESPONSE_BYTES + 1);
    for profile in [
        CreateReplyProfile::FreshCreate,
        CreateReplyProfile::WholeJoin,
        CreateReplyProfile::LegacyRetainedCreate,
    ] {
        assert_eq!(
            CreateReplyProfile::from_header(profile.header_value()),
            Some(profile)
        );
    }
    for value in ["", "bounded-create-v2", " bounded-create-v1", "legacy"] {
        assert_eq!(CreateReplyProfile::from_header(value), None);
    }
}

#[test]
fn complete_reply_range_counts_the_original_outer_response_codec() {
    // R5/R6/R10: full outgoing status/object or whole-join body, not a
    // single inner object/result. Exact B fits; +1 refuses while explicit
    // legacy encoding remains complete. No result/UID or JSON truncation.
    let overhead = serde_json::to_vec("").unwrap().len();
    for length in [RESPONSE_BYTES - 1, RESPONSE_BYTES, RESPONSE_BYTES + 1] {
        let value = "x".repeat(length - overhead);
        let bytes = CreateReplyProfile::FreshCreate.encode_body(&value);
        assert_eq!(bytes.is_ok(), length <= RESPONSE_BYTES);
        assert_eq!(
            CreateReplyProfile::LegacyRetainedCreate
                .encode_body(&value)
                .unwrap()
                .len(),
            length
        );
    }
}
