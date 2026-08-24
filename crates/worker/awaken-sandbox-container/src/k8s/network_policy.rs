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
const SANDBOX_ALLOWLIST_POLICY: &str = "awaken-sandbox-allowlist-egress";

/// Mutable live evidence owned by one runtime instance. All refresh and
/// fail-closed state transitions pass through this type.
pub(super) struct Attestation {
    network_none: AtomicBool,
    network_allowlist: AtomicBool,
}

impl Attestation {
    pub(super) const fn new() -> Self {
        Self {
            network_none: AtomicBool::new(false),
            network_allowlist: AtomicBool::new(false),
        }
    }

    pub(super) fn current(&self) -> bool {
        self.network_none.load(Ordering::Acquire)
    }

    pub(super) fn allowlist_current(&self) -> bool {
        self.network_allowlist.load(Ordering::Acquire)
    }

    pub(super) async fn refresh(
        &self,
        client: &kube::Client,
        namespace: &str,
    ) -> Result<(), RuntimeError> {
        let attested = match attest(client, namespace).await {
            Ok(attested) => attested,
            Err(error) => {
                self.network_none.store(false, Ordering::Release);
                self.network_allowlist.store(false, Ordering::Release);
                return Err(error);
            }
        };
        self.network_none
            .store(attested.network_none, Ordering::Release);
        self.network_allowlist
            .store(attested.network_allowlist, Ordering::Release);
        if attested.network_none {
            Ok(())
        } else {
            Err(backend(format!(
                "namespace {namespace:?} does not satisfy the canonical sandbox NetworkPolicy contract"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Evidence {
    network_none: bool,
    network_allowlist: bool,
}

pub(super) async fn attest(
    client: &kube::Client,
    namespace: &str,
) -> Result<Evidence, RuntimeError> {
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
    let policies = policies
        .list(&ListParams::default())
        .await
        .map_err(backend)?;
    Ok(Evidence {
        network_none: attests_contract(&policies.items),
        network_allowlist: attests_allowlist_contract(&policies.items),
    })
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

fn has_exact_namespace(selector: Option<&LabelSelector>) -> bool {
    selector.is_none_or(|selector| {
        selector
            .match_expressions
            .as_ref()
            .is_none_or(Vec::is_empty)
            && selector.match_labels.as_ref().is_some_and(|labels| {
                labels.len() == 1
                    && labels
                        .get("kubernetes.io/metadata.name")
                        .is_some_and(|namespace| !namespace.trim().is_empty())
            })
    })
}

fn attests_contract(policies: &[NetworkPolicy]) -> bool {
    let restricted = super::sandbox_network_labels(Some("restricted"));
    let open = super::sandbox_network_labels(Some("open"));

    let deny = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_DENY_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(&spec.pod_selector, &super::sandbox_network_labels(None))
                    && controls(policy, "Ingress")
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

fn attests_allowlist_contract(policies: &[NetworkPolicy]) -> bool {
    let allowlist = super::sandbox_network_labels(Some("allowlist"));
    let exact = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_ALLOWLIST_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(&spec.pod_selector, &allowlist)
                    && controls(policy, "Egress")
                    && spec.egress.as_ref().is_some_and(|rules| {
                        rules.len() == 1
                            && rules[0].to.as_ref().is_some_and(|peers| {
                                peers.len() == 1
                                    && peers[0].ip_block.is_none()
                                    && has_exact_namespace(peers[0].namespace_selector.as_ref())
                                    && peers[0].pod_selector.as_ref().is_some_and(|selector| {
                                        has_exact_labels(
                                            selector,
                                            &BTreeMap::from([(
                                                "app.kubernetes.io/component".to_owned(),
                                                "egress-gateway".to_owned(),
                                            )]),
                                        )
                                    })
                            })
                            && rules[0].ports.as_ref().is_some_and(|ports| {
                                ports.len() == 1
                                    && ports[0].protocol.as_deref() == Some("TCP")
                                    && ports[0].port.is_some()
                            })
                    })
            })
    });
    exact.is_some()
        && policies.iter().all(|policy| {
            policy.spec.as_ref().is_none_or(|spec| {
                !selector_matches(&spec.pod_selector, &allowlist)
                    || !allows_any_egress(policy)
                    || policy.metadata.name.as_deref() == Some(SANDBOX_ALLOWLIST_POLICY)
            })
        })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicyEgressRule, NetworkPolicyPeer, NetworkPolicyPort, NetworkPolicySpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelectorRequirement, ObjectMeta};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

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
            policy(
                SANDBOX_ALLOWLIST_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([
                        ("app".into(), "awaken-sandbox".into()),
                        ("awaken-egress".into(), "allowlist".into()),
                    ])),
                    ..Default::default()
                },
                &["Egress"],
                Some(vec![NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        namespace_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "kubernetes.io/metadata.name".into(),
                                "awaken-cloud".into(),
                            )])),
                            ..Default::default()
                        }),
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "app.kubernetes.io/component".into(),
                                "egress-gateway".into(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ports: Some(vec![NetworkPolicyPort {
                        protocol: Some("TCP".into()),
                        port: Some(IntOrString::Int(8081)),
                        end_port: None,
                    }]),
                }]),
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
        assert!(attests_allowlist_contract(&canonical), "P1 allowlist");
        assert!(!attests_contract(&canonical[1..]), "P2");

        let mut widened = canonical.clone();
        widened.push(policy(
            "dns-egress",
            LabelSelector::default(),
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        ));
        assert!(!attests_contract(&widened), "P3");
        assert!(!attests_allowlist_contract(&widened), "P3 allowlist");

        let mut wildcard_namespace = canonical.clone();
        wildcard_namespace[2]
            .spec
            .as_mut()
            .unwrap()
            .egress
            .as_mut()
            .unwrap()[0]
            .to
            .as_mut()
            .unwrap()[0]
            .namespace_selector = Some(LabelSelector::default());
        assert!(
            !attests_allowlist_contract(&wildcard_namespace),
            "a wildcard namespace can select an attacker-controlled gateway"
        );

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
