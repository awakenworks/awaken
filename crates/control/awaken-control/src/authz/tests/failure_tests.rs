use super::*;
use awaken_iam_server::SqlConn as _;

fn spec(id: &str) -> TokenSpec {
    TokenSpec {
        token_id: id.to_owned(),
        service_id: format!("service-{id}"),
        workspace_id: BOOTSTRAP_WORKSPACE.to_owned(),
        role: "admin".to_owned(),
        created_at: None,
        expires_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_service_principals_require_org_credential_management() {
    // C/E table: C1=target Workspace credential write is allowed; C2=request
    // selects a service identity; C3=the same PDP allows credential management
    // at this installation's Org. R1(C1+C2+!C3)->403 and no token/policy write;
    // R2(C1+!C2, omitted or empty)->201 with distinct server-owned identities;
    // R3(C1+C2+C3)->201 with
    // the requested identity. Org authority implies target authority for this
    // registered child Workspace; absent target authority is covered by MT8.
    // Existing PEP outage tests own Unavailable->503 and zero handler effects.
    let (dir, iam) = fresh_iam();
    let org_admin = admin_token(dir.path());
    let workspace_admin = mint(
        &iam,
        "workspace-credential-manager",
        BOOTSTRAP_WORKSPACE,
        "workspace_admin",
    );
    // Scope-kind legality and role authority are independent causes. The
    // canonical profile admits only exact apikey.write at Org; the same PDP
    // then requires an actual binding covering this installation's Org.
    // R4 Org actor + own Org write -> Allow; R5 Workspace actor + Org -> Deny;
    // R6 Org actor + Global/foreign Org -> Deny; R7 all other action families
    // at Org -> Deny. Existing Workspace route tests retain their whole matrix.
    let org_actor = iam.authenticate(&org_admin).unwrap().0;
    let workspace_actor = iam.authenticate(&workspace_admin).unwrap().0;
    let org_scope = ScopeRef::Org {
        org_id: iam.org_id.clone(),
    };
    assert_eq!(
        iam.authorize(org_actor.clone(), APIKEY_WRITE, org_scope.clone())
            .unwrap(),
        AuthorizationDecision::Allow,
        "R4 canonical profile and Org binding both permit credential write"
    );
    assert_eq!(
        iam.authorize(workspace_actor, APIKEY_WRITE, org_scope.clone())
            .unwrap(),
        AuthorizationDecision::Deny,
        "R5 scope declaration does not grant Org authority"
    );
    for scope in [
        ScopeRef::Global,
        ScopeRef::Org {
            org_id: OrgId("foreign-org-fixture".to_owned()),
        },
    ] {
        assert_eq!(
            iam.authorize(org_actor.clone(), APIKEY_WRITE, scope)
                .unwrap(),
            AuthorizationDecision::Deny,
            "R6 neither Global nor another Org is admitted"
        );
    }
    for action in [
        APIKEY_READ,
        WORKSPACE_READ,
        MODEL_SUPPLY_WRITE,
        FILE_WRITE,
        SKILL_WRITE,
        TUNNEL_MANAGE,
    ] {
        assert_eq!(
            iam.authorize(org_actor.clone(), action, org_scope.clone())
                .unwrap(),
            AuthorizationDecision::Deny,
            "R7 {action} remains Workspace-only"
        );
    }
    let before = iam.state.lock().unwrap().authz.snapshot();
    let before_tokens = iam.all_tokens().unwrap();
    let before_bindings = RoleBindingRepository::list(&iam.store).unwrap();
    let app = guarded_app(iam.clone());
    let (status, _) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&workspace_admin),
        Some(json!({
            "workspace_id": BOOTSTRAP_WORKSPACE,
            "role": "workspace_user",
            "principal_id": "selected-service-fixture"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "R1 requires Org authority");
    assert_eq!(iam.state.lock().unwrap().authz.snapshot(), before);
    assert_eq!(iam.all_tokens().unwrap(), before_tokens);
    assert_eq!(
        RoleBindingRepository::list(&iam.store).unwrap(),
        before_bindings
    );

    let mut generated_principals = Vec::new();
    for requested in [None, Some("")] {
        let mut body = json!({
            "workspace_id": BOOTSTRAP_WORKSPACE,
            "role": "workspace_user"
        });
        if let Some(requested) = requested {
            body["principal_id"] = json!(requested);
        }
        let (status, response) = call(
            &app,
            "POST",
            "/v1/config/iam/tokens",
            Some(&workspace_admin),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "R2 normal Workspace issuance");
        let generated = response["api_token"]["principal_id"].as_str().unwrap();
        assert!(!generated.is_empty());
        assert_ne!(generated, "selected-service-fixture");
        generated_principals.push(generated.to_owned());
    }
    assert_ne!(generated_principals[0], generated_principals[1]);

    let (status, response) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&org_admin),
        Some(json!({
            "workspace_id": BOOTSTRAP_WORKSPACE,
            "role": "workspace_user",
            "principal_id": "selected-service-fixture"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "R3 original Org administrator");
    assert_eq!(
        response["api_token"]["principal_id"],
        "selected-service-fixture"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn poisoned_embedded_state_is_unavailable_without_handler_or_mutation() {
    // C1 authenticated token existed; C2 the shared Directory/PDP guard is
    // poisoned. E1 authn and the real management PEP return Unavailable/503,
    // never 401/403/404; E2 mutation and workspace registration refuse without
    // reading partial state or clearing poison; E3 durable rows stay unchanged.
    let (dir, iam) = fresh_iam();
    let secret = admin_token(dir.path());
    let before = RoleBindingRepository::list(&iam.store).unwrap();
    let state = Arc::clone(&iam.state);
    assert!(
        std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("test-only partial-state poison");
        })
        .join()
        .is_err()
    );
    assert!(matches!(
        iam.token_by_id("missing"),
        Err(IamOperationError::StateUnavailable)
    ));
    assert!(matches!(
        iam.mint_token(spec("must-not-mint")),
        Err(IamOperationError::StateUnavailable)
    ));
    assert!(iam.register_workspace("must-not-register").is_err());
    let (status, body) = call(
        &guarded_app(iam.clone()),
        "GET",
        "/v1/config/catalog",
        Some(&secret),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["message"], "IAM is temporarily unavailable");
    assert_eq!(RoleBindingRepository::list(&iam.store).unwrap(), before);
    assert!(iam.state.is_poisoned());
}

#[tokio::test(flavor = "multi_thread")]
async fn token_mint_sql_refusals_leave_no_live_or_durable_half() {
    // C/E decision table over the REAL SQLite transaction and HTTP handler:
    // | first/second write or backend | HTTP | token + binding/live/version |
    // | token constraint            | 409  | unchanged                    |
    // | binding constraint          | 409  | rolled back + unchanged      |
    // | query-only backend          | 503  | unchanged                    |
    // | deferred commit constraint  | 409  | both real writes rolled back |
    // Constraints are business Conflict, never an invented retryable outage.
    // All rows prepare normally; policy_mut is reached only after commit ACK.
    let cases: &[(&[&str], StatusCode)] = &[
        (
            &[
                "CREATE TRIGGER refuse_token BEFORE INSERT ON iam_api_tokens BEGIN SELECT RAISE(ABORT, 'fixture token constraint'); END",
            ],
            StatusCode::CONFLICT,
        ),
        (
            &[
                "CREATE TRIGGER refuse_binding BEFORE INSERT ON iam_role_bindings BEGIN SELECT RAISE(ABORT, 'fixture binding constraint'); END",
            ],
            StatusCode::CONFLICT,
        ),
        (&["PRAGMA query_only=ON"], StatusCode::SERVICE_UNAVAILABLE),
        (
            &[
                "PRAGMA foreign_keys=ON",
                "CREATE TABLE fixture_parent(id INTEGER PRIMARY KEY)",
                "CREATE TABLE fixture_deferred(id INTEGER REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                "CREATE TRIGGER refuse_commit AFTER INSERT ON iam_role_bindings BEGIN INSERT INTO fixture_deferred VALUES (1); END",
            ],
            StatusCode::CONFLICT,
        ),
    ];
    for &(setup, expected) in cases {
        let (dir, iam) = fresh_iam();
        let before = iam.state.lock().unwrap().authz.snapshot();
        let tokens = iam.all_tokens().unwrap();
        let bindings = RoleBindingRepository::list(&iam.store).unwrap();
        for sql in setup {
            iam.store.backend().execute(sql, &[]).unwrap();
        }
        let principal = PrincipalRef::Service {
            service_id: "service-refused".into(),
        };
        let (status, body) = call(
            &guarded_app(iam.clone()), "POST", "/v1/config/iam/tokens", Some(&admin_token(dir.path())),
            Some(json!({"workspace_id": BOOTSTRAP_WORKSPACE, "role": "admin", "principal_id": "service-refused"})),
        ).await;
        assert_eq!(status, expected, "{body}");
        assert!(!body.to_string().contains("fixture"));
        assert_eq!(iam.all_tokens().unwrap(), tokens);
        assert_eq!(RoleBindingRepository::list(&iam.store).unwrap(), bindings);
        let state = iam.state.lock().unwrap();
        assert_eq!(state.authz.snapshot(), before);
        assert!(state.directory.list_for_principal(&principal).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn token_revoke_backend_refusal_preserves_both_then_replay_keeps_first_stamp() {
    // R1 genuine backend refusal -> 503 and both live/durable bearer remain
    // usable; R2 restore backend -> one committed stamp publishes; R3 replay
    // preserves that stamp, and restart still rejects the revoked credential.
    let (dir, iam) = fresh_iam();
    let secret = mint(&iam, "revoke-fault", BOOTSTRAP_WORKSPACE, "admin");
    let before = iam.token_by_id("revoke-fault").unwrap().unwrap();
    iam.store
        .backend()
        .execute("PRAGMA query_only=ON", &[])
        .unwrap();
    let app = guarded_app(iam.clone());
    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/revoke-fault",
        Some(&admin_token(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        iam.token_by_id("revoke-fault").unwrap(),
        Some(before.clone())
    );
    assert_eq!(
        ApiTokenRepository::get(&iam.store, &before.id).unwrap(),
        Some(before)
    );
    assert!(iam.authenticate(&secret).is_ok());
    iam.store
        .backend()
        .execute("PRAGMA query_only=OFF", &[])
        .unwrap();
    let revoked = iam.revoke_token("revoke-fault").unwrap();
    assert_eq!(
        iam.revoke_token("revoke-fault").unwrap().revoked_at,
        revoked.revoked_at
    );
    assert!(revoked.revoked_at.is_some());
    let restarted = embedded_iam(dir.path(), awaken_time::system_clock()).unwrap();
    assert!(matches!(
        restarted.authenticate(&secret),
        Err(AuthReject::Revoked)
    ));
}

#[test]
fn bootstrap_occupied_handoff_refuses_without_minting_or_overwriting() {
    // Reserved plain-bearer names are not evidence of a pending authorization:
    // existing file/directory/symlink/hardlink -> fatal refusal, no token/live
    // success, no overwrite/remint/adoption. This deliberately requires operator
    // repair if a prior uncertain DB commit left an uncorroborated handoff.
    for kind in ["file", "directory", "symlink", "hardlink"] {
        if cfg!(not(unix)) && kind == "symlink" {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ADMIN_TOKEN_FILE);
        let original = dir.path().join("operator-owned");
        std::fs::write(&original, b"operator credential must survive").unwrap();
        match kind {
            "file" => std::fs::write(&path, b"operator credential must survive").unwrap(),
            "directory" => std::fs::create_dir(&path).unwrap(),
            "hardlink" => std::fs::hard_link(&original, &path).unwrap(),
            #[cfg(unix)]
            "symlink" => std::os::unix::fs::symlink(&original, &path).unwrap(),
            _ => unreachable!(),
        }
        assert!(embedded_iam(dir.path(), awaken_time::system_clock()).is_err());
        let store = sqlite_migrated_store(
            SqliteBackend::open_path(dir.path().join("iam.sqlite")).unwrap(),
            "iam",
        )
        .unwrap();
        assert!(
            ApiTokenRepository::get(&store, &ApiTokenId("tok_mgmt_bootstrap".into()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"operator credential must survive"
        );
        if kind != "directory" {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"operator credential must survive"
            );
        }
        assert!(embedded_iam(dir.path(), awaken_time::system_clock()).is_err());
    }
}

#[test]
fn durable_handoff_precedes_sql_and_is_not_removed_on_refusal() {
    // R1 handoff publication fails -> zero token/binding/live changes;
    // R2 handoff succeeds then SQL refuses -> leave the durable secret intact,
    // no live publication; R3 SQL ACK -> same issued secret, owner-only file.
    // R2 is a known backend refusal; real ambiguous COMMIT may not be labelled
    // "rolled back" and the same no-compensation-delete law preserves evidence.
    let (dir, iam) = fresh_iam();
    let before = iam.state.lock().unwrap().authz.snapshot();
    let occupied = dir.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    assert!(matches!(
        iam.mint_token_with_handoff(spec("handoff-fail"), Some(&occupied)),
        Err(IamOperationError::Handoff(_))
    ));
    assert!(iam.token_by_id("handoff-fail").unwrap().is_none());
    assert!(
        ApiTokenRepository::get(&iam.store, &ApiTokenId("handoff-fail".into()))
            .unwrap()
            .is_none()
    );
    assert!(
        RoleBindingRepository::list_for_principal(
            &iam.store,
            &PrincipalRef::Service {
                service_id: "service-handoff-fail".into()
            }
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(iam.state.lock().unwrap().authz.snapshot(), before);
    iam.store
        .backend()
        .execute("PRAGMA query_only=ON", &[])
        .unwrap();
    let uncertain = dir.path().join("kept-handoff");
    assert!(matches!(
        iam.mint_token_with_handoff(spec("sql-fail"), Some(&uncertain)),
        Err(IamOperationError::Store(RepositoryError::Backend(_)))
    ));
    assert!(!std::fs::read(&uncertain).unwrap().is_empty());
    assert!(iam.token_by_id("sql-fail").unwrap().is_none());
    assert_eq!(iam.state.lock().unwrap().authz.snapshot(), before);
    iam.store
        .backend()
        .execute("PRAGMA query_only=OFF", &[])
        .unwrap();
    let success = dir.path().join("committed-handoff");
    let issued = iam
        .mint_token_with_handoff(spec("handoff-success"), Some(&success))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&success).unwrap(), issued.secret);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&success).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn existing_rotated_or_revoked_credentials_do_not_require_bootstrap_file() {
    // Existing durable credentials remain the authority: revoked bootstrap or
    // operator removal of its cleartext does not re-mint/bootstrap or deny an
    // otherwise healthy startup. A file never restores a removed role binding.
    for revoke_bootstrap in [false, true] {
        let (dir, iam) = fresh_iam();
        let successor = mint(&iam, "successor", BOOTSTRAP_WORKSPACE, "admin");
        if revoke_bootstrap {
            iam.revoke_token("tok_mgmt_bootstrap").unwrap();
        }
        std::fs::remove_file(dir.path().join(ADMIN_TOKEN_FILE)).unwrap();
        drop(iam);
        let restarted = embedded_iam(dir.path(), awaken_time::system_clock()).unwrap();
        assert!(restarted.authenticate(&successor).is_ok());
        assert!(!dir.path().join(ADMIN_TOKEN_FILE).exists());
        assert!(restarted.token_by_id("successor").unwrap().is_some());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn token_view_storage_failure_is_not_empty_or_not_found() {
    // Valid authentication and target authorization + unreadable real SQLite
    // binding rows -> 503, never an empty list/404. The response is generic and
    // does not disclose the SQL/table error to the caller. R2 DELETE resolves
    // live token identity first, then role_of must fail BEFORE revocation CAS:
    // generic 503 and the original live/durable token remain unrevoked.
    let (dir, iam) = fresh_iam();
    let secret = admin_token(dir.path());
    iam.store
        .backend()
        .execute("DROP TABLE iam_role_bindings", &[])
        .unwrap();
    let (status, body) = call(
        &guarded_app(iam.clone()),
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&secret),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["message"], "IAM is temporarily unavailable");
    assert!(!body.to_string().contains("iam_role_bindings"));
    let before = iam.token_by_id("tok_mgmt_bootstrap").unwrap().unwrap();
    assert!(before.revoked_at.is_none());
    let (status, body) = call(
        &guarded_app(iam.clone()),
        "DELETE",
        "/v1/config/iam/tokens/tok_mgmt_bootstrap",
        Some(&secret),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["message"], "IAM is temporarily unavailable");
    assert_eq!(
        iam.token_by_id("tok_mgmt_bootstrap").unwrap(),
        Some(before.clone())
    );
    assert_eq!(
        ApiTokenRepository::get(&iam.store, &before.id).unwrap(),
        Some(before)
    );
}

#[test]
fn invalid_native_mint_keeps_policy_version_and_rows_unchanged() {
    // Invalid expiry is decided by the one Core prepare law BEFORE touching
    // policy_mut (which increments its version), SQL, or a handoff file.
    let (_dir, iam) = fresh_iam();
    let before = iam.state.lock().unwrap().authz.snapshot();
    let tokens = iam.all_tokens().unwrap();
    let mut invalid = spec("invalid-window");
    invalid.created_at = Some("2030-01-01T00:00:00Z".into());
    invalid.expires_at = Some("2029-01-01T00:00:00Z".into());
    assert!(matches!(
        iam.mint_token(invalid),
        Err(IamOperationError::Token(
            IamError::InvalidApiTokenWindow { .. }
        ))
    ));
    assert_eq!(iam.state.lock().unwrap().authz.snapshot(), before);
    assert_eq!(iam.all_tokens().unwrap(), tokens);
}

#[test]
fn embedded_startup_io_refusal_never_returns_an_open_identity() {
    // A data-directory or database path occupied by the wrong physical kind
    // must return Err at the original constructor, before any usable identity
    // is returned. Retry uses the same path and is still a refusal, not Open.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, b"operator data").unwrap();
    assert!(embedded_iam(&file, awaken_time::system_clock()).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"operator data");
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(data.join("iam.sqlite")).unwrap();
    assert!(embedded_iam(&data, awaken_time::system_clock()).is_err());
    assert!(embedded_iam(&data, awaken_time::system_clock()).is_err());
    assert!(!data.join(ADMIN_TOKEN_FILE).exists());
}
