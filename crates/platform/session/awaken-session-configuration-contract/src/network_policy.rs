// @formal entrypoint session_creation.network_intersection_is_canonical_and_non_widening
//! Canonical frozen Session network policy and restriction algebra.

/// Frozen network fact. This is Session state, not a provider request; the Host
/// projects it to the provisioning contract at realization time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionNetworkPolicy {
    Unrestricted,
    Allowlist { hosts: Vec<String> },
    None,
}

impl SessionNetworkPolicy {
    /// Whether the frozen Session policy restricts egress at all.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }

    /// Canonical form used at every persistence/realization boundary. Host names
    /// are trimmed, lower-cased and deduplicated; an empty allowlist is exactly
    /// `None`, not a second spelling of closed networking.
    #[must_use]
    pub fn normalized(&self) -> Self {
        match self {
            Self::Unrestricted => Self::Unrestricted,
            Self::None => Self::None,
            Self::Allowlist { hosts } => {
                let mut hosts = hosts
                    .iter()
                    .map(|host| host.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>();
                // Equal keys are identical String values and will be deduplicated.
                // Their original order carries no authority; keep canonical byte
                // order without stable sorting's auxiliary allocation.
                hosts.sort_unstable();
                hosts.dedup();
                // Filtering the canonical list preserves order and uniqueness.
                // Collecting before filtering preserves the input length through sorting.
                hosts.retain(|host| !host.is_empty());
                Self::from_canonical_hosts(hosts)
            }
        }
    }

    /// Safe meet for independently authored Session restrictions. The result
    /// never widens either input and canonicalizes an empty intersection to
    /// `None`.
    #[must_use]
    pub fn safe_intersection(&self, other: &Self) -> Self {
        match (self.normalized(), other.normalized()) {
            (Self::None, _) | (_, Self::None) => Self::None,
            (Self::Unrestricted, policy) | (policy, Self::Unrestricted) => policy,
            (Self::Allowlist { hosts: left }, Self::Allowlist { hosts: right }) => {
                let hosts = left
                    .into_iter()
                    .filter(|host| right.binary_search(host).is_ok())
                    .collect::<Vec<_>>();
                // Filtering the canonical left list preserves its order,
                // uniqueness and spelling; only the empty-result rule remains.
                Self::from_canonical_hosts(hosts)
            }
        }
    }

    /// Both callers already own sorted, unique, nonblank canonical host names.
    /// Keep the empty allowlist's sole representation at this one boundary.
    fn from_canonical_hosts(hosts: Vec<String>) -> Self {
        if hosts.is_empty() {
            Self::None
        } else {
            Self::Allowlist { hosts }
        }
    }
}

#[cfg(any(test, kani))]
mod collection_tests;

#[cfg(test)]
mod tests {
    use super::SessionNetworkPolicy;
    use proptest::prelude::*;

    fn policy() -> impl Strategy<Value = SessionNetworkPolicy> {
        prop_oneof![
            Just(SessionNetworkPolicy::None),
            Just(SessionNetworkPolicy::Unrestricted),
            proptest::collection::vec(any::<String>(), 0..16)
                .prop_map(|hosts| SessionNetworkPolicy::Allowlist { hosts }),
        ]
    }

    proptest! {
        // All enum pairs/triples, arbitrary UTF-8 host spellings and duplicate
        // or empty lists: normalization is idempotent; meet is canonical,
        // commutative, associative and idempotent, and grants no host absent
        // from either normalized operand. None absorbs and top is identity.
        // Bounds here are test generation limits, not product input limits or
        // a universal Rust refinement claim. N7 below fixes the blank witness.
        #[test]
        fn network_restriction_obeys_canonical_meet_laws(left in policy(), right in policy(), third in policy()) {
            let left_normal = left.normalized();
            let right_normal = right.normalized();
            let result = left.safe_intersection(&right);
            prop_assert_eq!(left_normal.normalized(), left_normal.clone());
            prop_assert_eq!(result.normalized(), result.clone());
            prop_assert_eq!(result.clone(), right.safe_intersection(&left));
            prop_assert_eq!(left.safe_intersection(&left), left_normal.clone());
            prop_assert_eq!(result.safe_intersection(&third), left.safe_intersection(&right.safe_intersection(&third)));
            prop_assert_eq!(left.safe_intersection(&SessionNetworkPolicy::None), SessionNetworkPolicy::None);
            prop_assert_eq!(left.safe_intersection(&SessionNetworkPolicy::Unrestricted), left_normal.clone());
            match result {
                SessionNetworkPolicy::Unrestricted => {
                    prop_assert_eq!(left_normal, SessionNetworkPolicy::Unrestricted);
                    prop_assert_eq!(right_normal, SessionNetworkPolicy::Unrestricted);
                }
                SessionNetworkPolicy::None => {}
                SessionNetworkPolicy::Allowlist { hosts } => {
                    for input in [&left_normal, &right_normal] {
                        match input {
                            SessionNetworkPolicy::Unrestricted => {}
                            SessionNetworkPolicy::None => prop_assert!(false, "None cannot grant a host"),
                            SessionNetworkPolicy::Allowlist { hosts: allowed } => {
                                prop_assert!(hosts.iter().all(|host| allowed.contains(host)));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn network_safe_intersection_cases_follow_the_decision_table() {
        // Cause graph:
        // either None -> None; otherwise either Unrestricted -> canonicalized
        // other side; otherwise intersect canonical host identities; an empty
        // intersection -> None. No branch may widen either input.
        //
        // | Rule | Left | Right | Shared hosts | Effect |
        // |---|---|---|---|---|
        // | N1 | Unrestricted | Unrestricted | - | Unrestricted |
        // | N2 | Unrestricted | Allowlist(B,a,A) | - | Allowlist(a,b) |
        // | N3 | Allowlist(B,a,A) | Unrestricted | - | Allowlist(a,b) |
        // | N4 | None | any | - | None |
        // | N5 | Allowlist(API,db) | Allowlist(api,cache) | api | Allowlist(api) |
        // | N6 | Allowlist(api) | Allowlist(db) | none | None |
        // | N7 | blank-only Allowlist | blank-only Allowlist | no real host | None |
        let blank = SessionNetworkPolicy::Allowlist {
            hosts: vec![" ".into(), "\t".into()],
        };
        assert_eq!(
            blank.safe_intersection(&blank),
            SessionNetworkPolicy::None,
            "N7"
        );
        assert_eq!(
            serde_json::to_value(blank.safe_intersection(&blank)).unwrap(),
            serde_json::json!({"mode": "none"}),
            "N7 wire"
        );
        let unrestricted = SessionNetworkPolicy::Unrestricted;
        let mixed = SessionNetworkPolicy::Allowlist {
            hosts: vec!["B.example".into(), "a.example".into(), "A.EXAMPLE".into()],
        };
        let canonical = SessionNetworkPolicy::Allowlist {
            hosts: vec!["a.example".into(), "b.example".into()],
        };
        assert_eq!(
            unrestricted.safe_intersection(&unrestricted),
            SessionNetworkPolicy::Unrestricted,
            "N1"
        );
        assert_eq!(unrestricted.safe_intersection(&mixed), canonical, "N2");
        assert_eq!(mixed.safe_intersection(&unrestricted), canonical, "N3");
        assert_eq!(
            SessionNetworkPolicy::None.safe_intersection(&mixed),
            SessionNetworkPolicy::None,
            "N4"
        );
        assert_eq!(
            SessionNetworkPolicy::Allowlist {
                hosts: vec![" API.EXAMPLE ".into(), "db.example".into()],
            }
            .safe_intersection(&SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into(), "cache.example".into()],
            }),
            SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            },
            "N5"
        );
        assert_eq!(
            SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            }
            .safe_intersection(&SessionNetworkPolicy::Allowlist {
                hosts: vec!["db.example".into()],
            }),
            SessionNetworkPolicy::None,
            "N6"
        );
    }
}
