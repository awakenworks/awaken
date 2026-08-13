//! Release-contract projection for the product-owned Control authorization
//! profile.
//!
//! Causal decision table:
//! | invocation | config/storage/network | outcome |
//! | --- | --- | --- |
//! | `control iam profile` | unavailable | deterministic profile JSON |
//! | `control iam profile resources` | unavailable | deterministic resource profile JSON |
//! | `control iam profile runtime` | unavailable | deterministic Hosted lifecycle profile JSON |
//! | same invocation twice | unavailable | byte-identical JSON |
//! | extra argument | unavailable | usage failure, no profile |

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
    assert_eq!(
        profile["namespace"],
        serde_json::json!("awaken.runtime.management")
    );
    let grants = profile["document"]["grants"]
        .as_array()
        .expect("profile grants");
    let publisher = grants
        .iter()
        .filter(|grant| {
            grant["subject"]["role_id"]
                == serde_json::json!("awaken.runtime.management:agent_publisher")
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
            "awaken.runtime.management::workspace.*",
            "awaken.runtime.management::model_supply.read",
        ]
    );
    assert!(publisher_actions.iter().all(|action| {
        !action.contains("apikey")
            && !action.ends_with("model_supply.*")
            && !action.ends_with("model_supply.connect")
            && !action.ends_with("model_supply.write")
    }));
}

#[test]
fn control_resource_profile_is_a_side_effect_free_release_projection() {
    let binary = env!("CARGO_BIN_EXE_awaken");
    let first = Command::new(binary)
        .args(["control", "iam", "profile", "resources"])
        .env("AWAKEN_CONFIG", "/does/not/exist")
        .output()
        .expect("run resource profile projection");
    let second = Command::new(binary)
        .args(["control", "iam", "profile", "resources"])
        .env("AWAKEN_CONFIG", "/also/does/not/exist")
        .output()
        .expect("run resource profile projection again");

    assert!(first.status.success(), "{:?}", first.stderr);
    assert!(second.status.success(), "{:?}", second.stderr);
    assert_eq!(first.stdout, second.stdout);

    let profile: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("resource profile is JSON");
    assert_eq!(
        profile["namespace"],
        serde_json::json!("awaken.runtime.resources")
    );
    let actions = profile["document"]["resource_model"]["actions"]
        .as_array()
        .expect("resource actions");
    assert!(
        actions
            .iter()
            .any(|action| action == "awaken.runtime.resources::file.*")
    );
    assert!(
        actions
            .iter()
            .any(|action| action == "awaken.runtime.resources::skill.*")
    );
    assert!(
        actions
            .iter()
            .all(|action| !action.as_str().unwrap_or_default().contains("apikey"))
    );
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
fn control_profile_rejects_an_ambiguous_invocation() {
    let output = Command::new(env!("CARGO_BIN_EXE_awaken"))
        .args(["control", "iam", "profile", "extra"])
        .output()
        .expect("run invalid profile projection");

    assert!(!output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("awaken.runtime.management"),
        "usage output must not contain a profile"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "control iam requires `profile`, `profile resources`, or `profile runtime` exactly"
    ));
}
