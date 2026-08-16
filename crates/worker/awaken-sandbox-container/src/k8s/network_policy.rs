//! Live Kubernetes NetworkPolicy attestation for the canonical sandbox labels.
//!
//! NetworkPolicy resources remain the enforcement authority. This module only
//! proves that the live namespace contains the exact deny/open contract and
//! that no additive egress policy widens a restricted sandbox.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::Api;
use kube::api::ListParams;

use super::error::backend;
use crate::RuntimeError;

const SANDBOX_DENY_POLICY: &str = "awaken-sandbox-default-deny";
const SANDBOX_OPEN_POLICY: &str = "awaken-sandbox-open-egress";

/// Mutable live evidence owned by one runtime instance. All refresh and
/// fail-closed state transitions pass through this type.
pub(super) struct Attestation(AtomicBool);

impl Attestation {
    pub(super) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    pub(super) fn current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(super) async fn refresh(
        &self,
        client: &kube::Client,
        namespace: &str,
    ) -> Result<(), RuntimeError> {
        let attested = match attest(client, namespace).await {
            Ok(attested) => attested,
            Err(error) => {
                self.0.store(false, Ordering::Release);
                return Err(error);
            }
        };
        self.0.store(attested, Ordering::Release);
        if attested {
            Ok(())
        } else {
            Err(backend(format!(
                "namespace {namespace:?} does not satisfy the canonical sandbox NetworkPolicy contract"
            )))
        }
    }
}

pub(super) async fn attest(client: &kube::Client, namespace: &str) -> Result<bool, RuntimeError> {
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
    let policies = policies
        .list(&ListParams::default())
        .await
        .map_err(backend)?;
    Ok(attests_contract(&policies.items))
}

fn selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
    if !selector.match_labels.as_ref().is_none_or(|expected| {
        expected
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }) {
        return false;
    }
    selector
        .match_expressions
        .as_ref()
        .is_none_or(|requirements| {
            requirements.iter().all(|requirement| {
                let value = labels.get(&requirement.key);
                let values = requirement.values.as_deref().unwrap_or_default();
                match requirement.operator.as_str() {
                    "In" => value.is_some_and(|value| values.iter().any(|item| item == value)),
                    "NotIn" => value.is_some_and(|value| values.iter().all(|item| item != value)),
                    "Exists" => value.is_some(),
                    "DoesNotExist" => value.is_none(),
                    _ => false,
                }
            })
        })
}

fn controls(policy: &NetworkPolicy, policy_type: &str) -> bool {
    policy.spec.as_ref().is_some_and(|spec| {
        spec.policy_types
            .as_ref()
            .is_some_and(|types| types.iter().any(|candidate| candidate == policy_type))
            || (policy_type == "Ingress" && spec.ingress.is_some())
            || (policy_type == "Egress" && spec.egress.is_some())
    })
}

fn allows_any_egress(policy: &NetworkPolicy) -> bool {
    policy
        .spec
        .as_ref()
        .and_then(|spec| spec.egress.as_ref())
        .is_some_and(|rules| !rules.is_empty())
}

fn has_exact_labels(selector: &LabelSelector, expected: &BTreeMap<String, String>) -> bool {
    selector.match_labels.as_ref() == Some(expected)
        && selector
            .match_expressions
            .as_ref()
            .is_none_or(Vec::is_empty)
}

fn attests_contract(policies: &[NetworkPolicy]) -> bool {
    let restricted = BTreeMap::from([
        ("app".to_owned(), "awaken-sandbox".to_owned()),
        ("awaken-egress".to_owned(), "restricted".to_owned()),
    ]);
    let open = BTreeMap::from([
        ("app".to_owned(), "awaken-sandbox".to_owned()),
        ("awaken-egress".to_owned(), "open".to_owned()),
    ]);

    let deny = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_DENY_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(
                    &spec.pod_selector,
                    &BTreeMap::from([("app".to_owned(), "awaken-sandbox".to_owned())]),
                ) && controls(policy, "Ingress")
                    && controls(policy, "Egress")
                    && spec.ingress.as_ref().is_none_or(Vec::is_empty)
                    && spec.egress.as_ref().is_none_or(Vec::is_empty)
            })
    });
    let open_allow = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_OPEN_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(&spec.pod_selector, &open)
                    && controls(policy, "Egress")
                    && spec.egress.as_ref().is_some_and(|rules| {
                        rules.len() == 1 && rules[0].to.is_none() && rules[0].ports.is_none()
                    })
            })
    });
    let restricted_is_not_widened = policies.iter().all(|policy| {
        policy.spec.as_ref().is_none_or(|spec| {
            !selector_matches(&spec.pod_selector, &restricted) || !allows_any_egress(policy)
        })
    });

    deny.is_some() && open_allow.is_some() && restricted_is_not_widened
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::networking::v1::{NetworkPolicyEgressRule, NetworkPolicySpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelectorRequirement, ObjectMeta};

    use super::*;

    fn policy(
        name: &str,
        selector: LabelSelector,
        policy_types: &[&str],
        egress: Option<Vec<NetworkPolicyEgressRule>>,
    ) -> NetworkPolicy {
        NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: selector,
                policy_types: Some(policy_types.iter().map(|value| (*value).into()).collect()),
                ingress: None,
                egress,
            }),
        }
    }

    fn canonical() -> Vec<NetworkPolicy> {
        vec![
            policy(
                SANDBOX_DENY_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([("app".into(), "awaken-sandbox".into())])),
                    ..Default::default()
                },
                &["Ingress", "Egress"],
                Some(Vec::new()),
            ),
            policy(
                SANDBOX_OPEN_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([
                        ("app".into(), "awaken-sandbox".into()),
                        ("awaken-egress".into(), "open".into()),
                    ])),
                    ..Default::default()
                },
                &["Egress"],
                Some(vec![NetworkPolicyEgressRule::default()]),
            ),
        ]
    }

    #[test]
    fn exact_live_policy_graph_is_required_without_additive_widening() {
        // Cause/effect graph: C1 exact sandbox deny exists; C2 open-only allow
        // exists; C3 another egress policy selects restricted Pods. Effect E1
        // advertise/admit isolation iff C1+C2+!C3. Rules: P1 canonical=>E1;
        // P2 missing deny=>reject; P3 namespace-wide allow=>reject; P4 an
        // explicit sandbox exclusion preserves E1.
        let canonical = canonical();
        assert!(attests_contract(&canonical), "P1");
        assert!(!attests_contract(&canonical[1..]), "P2");

        let mut widened = canonical.clone();
        widened.push(policy(
            "dns-egress",
            LabelSelector::default(),
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        ));
        assert!(!attests_contract(&widened), "P3");

        let mut excluded = canonical;
        excluded.push(policy(
            "dns-egress",
            LabelSelector {
                match_expressions: Some(vec![LabelSelectorRequirement {
                    key: "app".into(),
                    operator: "NotIn".into(),
                    values: Some(vec!["awaken-sandbox".into()]),
                }]),
                ..Default::default()
            },
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        ));
        assert!(attests_contract(&excluded), "P4");
    }
}
