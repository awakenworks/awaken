//! Release-contract projection for the product-owned Control authorization
//! profile.
//!
//! Causal decision table:
//! | invocation | config/storage/network | outcome |
//! | --- | --- | --- |
//! | `control iam profile` | unavailable | deterministic Workspace profile JSON |
//! | `control iam profile runtime` | unavailable | deterministic Hosted lifecycle profile JSON |
//! | `control surface profile runtime` | unavailable | deterministic schema-v2 routes plus canonical application-access TTL |
//! | same invocation twice | unavailable | byte-identical JSON |
//! | `profile resources` or extra argument | unavailable | usage failure, no profile |

use std::process::Command;

#[test]
fn control_profile_is_a_side_effect_free_release_projection() {
    let binary = env!("CARGO_BIN_EXE_awaken");
    let first = Command::new(binary)
        .args(["control", "iam", "profile"])
        .env("AWAKEN_CONFIG", "/does/not/exist")
        .output()
        .expect("run profile projection");
    let second = Command::new(binary)
        .args(["control", "iam", "profile"])
        .env("AWAKEN_CONFIG", "/also/does/not/exist")
        .output()
        .expect("run profile projection again");

    assert!(first.status.success(), "{:?}", first.stderr);
    assert!(second.status.success(), "{:?}", second.stderr);
    assert_eq!(first.stdout, second.stdout);

    let profile: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("profile is JSON");
    assert_eq!(profile["namespace"], serde_json::json!("awaken.workspace"));
    let grants = profile["document"]["grants"]
        .as_array()
        .expect("profile grants");
    let publisher = grants
        .iter()
        .filter(|grant| {
            grant["subject"]["role_id"] == serde_json::json!("awaken.workspace:publisher")
        })
        .collect::<Vec<_>>();
    // Release-projection decision table: the Flow publisher needs Workspace
    // authoring and read-only executable-model discovery; credential access,
    // model-supply mutation, and a broad model wildcard remain absent. This
    // separately verifies that CLI serialization does not lose either grant
    // from the authoritative in-process profile test.
    let publisher_actions = publisher
        .iter()
        .map(|grant| grant["action_pattern"].as_str().expect("action pattern"))
        .collect::<Vec<_>>();
    assert_eq!(
        publisher_actions,
        [
            "awaken.workspace::workspace.*",
            "awaken.workspace::model_supply.read",
            "awaken.workspace::skill.*",
        ]
    );
    assert!(publisher_actions.iter().all(|action| {
        !action.contains("apikey")
            && !action.ends_with("model_supply.*")
            && !action.ends_with("model_supply.connect")
            && !action.ends_with("model_supply.write")
    }));
    let credential_ingress = grants
        .iter()
        .filter(|grant| {
            grant["subject"]["role_id"] == serde_json::json!("awaken.workspace:credential_ingress")
        })
        .map(|grant| grant["action_pattern"].as_str().expect("action pattern"))
        .collect::<Vec<_>>();
    assert_eq!(credential_ingress, ["awaken.workspace::apikey.*"]);

    let actions = profile["document"]["resource_model"]["actions"]
        .as_array()
        .expect("Workspace actions");
    for expected in [
        "awaken.workspace::workspace.*",
        "awaken.workspace::file.*",
        "awaken.workspace::skill.*",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "{expected}"
        );
    }
}

#[test]
fn hosted_runtime_profile_is_a_side_effect_free_release_projection() {
    let binary = env!("CARGO_BIN_EXE_awaken");
    let project = || {
        Command::new(binary)
            .args(["control", "iam", "profile", "runtime"])
            .env("AWAKEN_CONFIG", "/does/not/exist")
            .output()
            .expect("run Hosted Runtime profile projection")
    };
    let first = project();
    let second = project();
    assert!(first.status.success(), "{:?}", first.stderr);
    assert_eq!(first.stdout, second.stdout);
    let profile: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("Runtime profile is JSON");
    assert_eq!(profile["namespace"], serde_json::json!("awaken.runtime"));
    let grants = profile["document"]["grants"]
        .as_array()
        .expect("Runtime grants");
    assert!(grants.iter().any(|grant| {
        grant["subject"]["role_id"] == "awaken.runtime:agent_executor"
            && grant["action_pattern"] == "awaken.runtime::run.*"
    }));
}

#[test]
fn hosted_runtime_surface_profile_projects_the_canonical_application_access_limit() {
    // Cause/effect graph: C1 the surface profile command runs without runtime
    // config/storage/network -> E1 emit deterministic JSON; C2 Coordinator's
    // canonical application-access maximum is positive and remains the legacy
    // v1 incumbent ceiling during the one-time durable cutover -> E2 emit that
    // exact constant as the required schema-v2 field, never a copied candidate;
    // C3 Control's route authority is populated -> E3 preserve its route
    // projection in the same artifact.
    //
    // Decision table:
    // | rule | command | canonical TTL | routes | outcome |
    // | S1 | surface profile runtime | positive canonical incumbent ceiling | populated | schema v2 with exact TTL and routes |
    // | S2 | same command twice | positive | populated | byte-identical JSON |
    let binary = env!("CARGO_BIN_EXE_awaken");
    let project = || {
        Command::new(binary)
            .args(["control", "surface", "profile", "runtime"])
            .env("AWAKEN_CONFIG", "/does/not/exist")
            .output()
            .expect("run Hosted Runtime surface profile projection")
    };
    let first = project();
    let second = project();
    assert!(first.status.success(), "{:?}", first.stderr);
    assert!(second.status.success(), "{:?}", second.stderr);
    assert_eq!(first.stdout, second.stdout);

    let profile: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("Runtime surface profile is JSON");
    assert_eq!(profile["schema_version"], serde_json::json!(2));
    assert_eq!(
        profile["application_access_max_ttl_seconds"],
        serde_json::json!(
            awaken_coordinator::application_access::APPLICATION_ACCESS_MAX_TTL_SECONDS
        )
    );
    assert!(
        profile["application_access_max_ttl_seconds"]
            .as_u64()
            .is_some_and(|seconds| seconds > 0)
    );
    assert!(
        !profile["routes"]
            .as_array()
            .expect("Runtime surface routes")
            .is_empty()
    );
}

#[test]
fn control_profile_rejects_an_ambiguous_invocation() {
    let output = Command::new(env!("CARGO_BIN_EXE_awaken"))
        .args(["control", "iam", "profile", "extra"])
        .output()
        .expect("run invalid profile projection");

    assert!(!output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("awaken.workspace"),
        "usage output must not contain a profile"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("control iam requires `profile` or `profile runtime` exactly")
    );

    let retired = Command::new(env!("CARGO_BIN_EXE_awaken"))
        .args(["control", "iam", "profile", "resources"])
        .output()
        .expect("run retired resource projection");
    assert!(!retired.status.success());
    assert!(
        !String::from_utf8_lossy(&retired.stdout).contains("\"namespace\""),
        "usage help may be printed, but no compatibility profile is emitted"
    );
}
