use super::*;
use awaken_container_contract::CreateReplyProfile;

#[tokio::test]
async fn original_body_primitive_bounds_complete_fresh_data_without_legacy_truncation() {
    let response_bytes = CreateReplyProfile::FreshCreate.body_limit().unwrap();
    // R3/R4/R9/V4: complete raw body B-1/B/B+1 is accepted/accepted/error;
    // no truncation or Content-Length assumption. Explicit legacy body
    // keeps its old complete bytes; only exact known markers select that
    // exception. Unknown markers are not a compatibility fallback.
    for length in [response_bytes - 1, response_bytes, response_bytes + 1] {
        let result = collect_create_body(
            Body::from(vec![b'x'; length]),
            CreateReplyProfile::FreshCreate,
        )
        .await;
        assert_eq!(result.is_ok(), length <= response_bytes);
        if let Ok(bytes) = result {
            assert_eq!(bytes.len(), length);
        }
    }
    let legacy = collect_create_body(
        Body::from(vec![b'x'; response_bytes + 1]),
        CreateReplyProfile::LegacyRetainedCreate,
    )
    .await
    .unwrap();
    assert_eq!(legacy.len(), response_bytes + 1);
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
