use super::*;

#[test]
fn management_authentication_accepts_only_the_canonical_token_scheme() {
    // Cause/effect graph: C1 the persisted credential is valid; C2 its presented
    // scheme is canonical. E1 authenticate the exact principal/Workspace; E2
    // reject before the IAM compatibility parser can reinterpret another
    // provider's prefix.
    //
    // Decision table:
    // | valid credential | scheme      | effect |
    // | yes              | sk-awaken-  | E1     |
    // | yes              | sk-ant-     | E2     |
    // | no               | any         | E2     |
    let dir = tempfile::tempdir().unwrap();
    let iam = embedded_iam(dir.path());
    let canonical = std::fs::read_to_string(dir.path().join(ADMIN_TOKEN_FILE)).unwrap();
    let canonical = canonical.trim();
    assert!(canonical.starts_with(CANONICAL_API_TOKEN_PREFIX));
    assert!(iam.authenticate(canonical).is_ok());

    let legacy = canonical.replacen(CANONICAL_API_TOKEN_PREFIX, "sk-ant-", 1);
    assert!(matches!(
        iam.authenticate(&legacy),
        Err(AuthReject::Invalid)
    ));
    assert!(matches!(
        iam.authenticate("not-a-token"),
        Err(AuthReject::Invalid)
    ));
}
