use super::*;

#[test]
fn hosted_runtime_routes_are_projected_from_the_authorization_table() {
    // Cause/effect graph: C1 an IAM route family is Coordinator-exclusive ->
    // E1 export its flat prefix and workspace wrapper; C2 a family is
    // Control-exclusive -> E2 omit both spellings; C3 Environment is shared ->
    // E3 export only its `/work` template in flat and wrapped form; C4 the
    // composition root supplies a positive application-access TTL -> E4
    // preserve that exact limit as a required schema-v2 field.
    // The export is derived from ROUTE_POLICIES, so adding an authenticated
    // Coordinator family without classifying its hosted owner fails this exact
    // decision table instead of silently creating a Cloud-owned second list.
    //
    // Decision table:
    // | rule | family owner | canonical matcher | exported matchers |
    // | R1 | Coordinator | path prefix | flat prefix + Workspace template |
    // | R2 | Control | any | absent |
    // | R3 | split | Environment work template | flat + two-parameter Workspace template |
    // | R4 | composition TTL | positive | exact schema-v2 field |
    let expected_flat = [
        ("prefix", "/v1/sessions"),
        ("prefix", "/v1/dreams"),
        ("prefix", "/v1/a2a"),
        ("prefix", "/v1/message:send"),
        ("prefix", "/v1/message:stream"),
        ("prefix", "/v1/ai-sdk"),
        ("prefix", "/v1/ag-ui"),
        ("prefix", "/v1/durable"),
        ("prefix", "/v1/files"),
        ("prefix", "/v1/skills"),
        ("prefix", "/v1/memory_stores"),
        ("prefix", "/v1/models"),
        ("prefix", "/v1/awaken/sessions"),
        ("prefix", "/v1/application-access-tokens"),
        ("prefix", "/v1/deployments"),
        ("prefix", "/v1/deployment_runs"),
        ("template", "/v1/environments/{environment_id}/work"),
        ("prefix", "/v1/awaken/memory-stores"),
    ];
    let application_access_max_ttl_seconds = std::num::NonZeroU64::new(37).unwrap();
    let first = hosted_runtime_route_profile(application_access_max_ttl_seconds);
    let second = hosted_runtime_route_profile(application_access_max_ttl_seconds);
    assert_eq!(first.schema_version, 2);
    assert_eq!(
        first.application_access_max_ttl_seconds,
        application_access_max_ttl_seconds
    );
    assert_eq!(first.routes.len(), expected_flat.len() * 2);
    for (pair, (kind, flat_path)) in first.routes.chunks_exact(2).zip(expected_flat) {
        let expected_flat_match = match kind {
            "prefix" => HostedRuntimePathMatch::PathPrefix {
                path: flat_path.to_owned(),
            },
            "template" => HostedRuntimePathMatch::PathTemplate {
                path_template: flat_path.to_owned(),
            },
            _ => unreachable!(),
        };
        assert_eq!(&pair[0], &expected_flat_match);
        assert_eq!(
            &pair[1],
            &HostedRuntimePathMatch::PathTemplate {
                path_template: format!(
                    "/v1/workspaces/{{workspace_id}}{}",
                    flat_path.strip_prefix("/v1").unwrap()
                ),
            }
        );
    }
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    for control_path in [
        "/v1/config",
        "/v1/vaults",
        "/v1/agents",
        "/v1/user_profiles",
        "/v1/awaken/environments",
    ] {
        assert!(!first.routes.iter().any(|route| {
            matches!(route, HostedRuntimePathMatch::PathPrefix { path } if path == control_path)
                || matches!(route, HostedRuntimePathMatch::PathTemplate { path_template } if path_template.ends_with(control_path))
        }));
    }
    assert!(
        first
            .routes
            .contains(&HostedRuntimePathMatch::PathTemplate {
                path_template: "/v1/workspaces/{workspace_id}/environments/{environment_id}/work"
                    .to_owned(),
            })
    );
    for runtime_path in [
        "/v1/dreams",
        "/v1/ai-sdk/threads/thread_1/runs",
        "/v1/ag-ui",
        "/v1/durable/threads/thread_1/dispatches",
        "/v1/models",
        "/v1/awaken/sessions/session_1/live-inbox",
    ] {
        assert!(super::action_for(&Method::GET, runtime_path).is_some());
    }
}
