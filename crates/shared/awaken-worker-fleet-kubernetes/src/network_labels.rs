//! The labels the composition-owned Sandbox NetworkPolicies select on.
//!
//! Sandbox Pods and the package-image Jobs that run beside them carry the same
//! labels, so one deployed policy graph bounds both.

use std::collections::BTreeMap;

/// Canonical labels selected by the composition-owned sandbox NetworkPolicies.
/// `None` selects only the default deny policy; a posture additionally selects
/// the matching egress policy.
pub fn sandbox_network_labels(posture: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([("app".to_owned(), "awaken-sandbox".to_owned())]);
    if let Some(posture) = posture {
        labels.insert("awaken-egress".to_owned(), posture.to_owned());
    }
    labels
}
