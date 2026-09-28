//! Finite decision kernel for one aggregate-authored live Resource transition.
//!
//! The parent module remains the sole physical executor. This module owns no
//! projection state or effects; it only prevents empty desired state from being
//! confused with an absent command or an ordinary replacement.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LiveResourceProjectionChange {
    Retain,
    Replace,
    Revoke,
}

impl LiveResourceProjectionChange {
    pub(super) const fn removes_previous(self) -> bool {
        !matches!(self, Self::Retain)
    }

    pub(super) const fn installs_desired(self) -> bool {
        matches!(self, Self::Replace)
    }
}

/// Classify the complete desired manifest without inspecting process-local
/// projection state. A changed, explicitly empty generation is a revocation;
/// it is never equivalent to retaining the previous projection.
pub(super) const fn live_resource_projection_change(
    generation_changed: bool,
    desired_has_inputs: bool,
    desired_has_skills: bool,
) -> LiveResourceProjectionChange {
    if !generation_changed {
        LiveResourceProjectionChange::Retain
    } else if desired_has_inputs || desired_has_skills {
        LiveResourceProjectionChange::Replace
    } else {
        LiveResourceProjectionChange::Revoke
    }
}

#[cfg(kani)]
/// @obligation session_resources.explicit_empty_manifest_revokes_runtime_projection
#[kani::proof]
fn explicit_empty_resource_manifest_revokes_without_activation() {
    // Cause/effect graph: C1 the aggregate generation changed; C2 the desired
    // manifest has inputs; C3 it has Skills. E1 retain performs no removal or
    // target installation; E2 replacement removes old state and installs the
    // nonempty target; E3 explicit empty removes old state and installs no
    // target. Constraints: inputs/Skills are the complete manifest payload.
    // Decision rules: R1=!C1=>E1; R2=C1&&(C2||C3)=>E2;
    // R3=C1&&!C2&&!C3=>E3.
    let generation_changed: bool = kani::any();
    let desired_has_inputs: bool = kani::any();
    let desired_has_skills: bool = kani::any();
    let change =
        live_resource_projection_change(generation_changed, desired_has_inputs, desired_has_skills);
    let explicit_empty = !desired_has_inputs && !desired_has_skills;

    assert_eq!(
        change == LiveResourceProjectionChange::Retain,
        !generation_changed,
    );
    assert_eq!(
        change == LiveResourceProjectionChange::Replace,
        generation_changed && !explicit_empty,
    );
    assert_eq!(
        change == LiveResourceProjectionChange::Revoke,
        generation_changed && explicit_empty,
    );
    if change == LiveResourceProjectionChange::Revoke {
        assert!(change.removes_previous());
        assert!(!change.installs_desired());
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveResourceProjectionChange, live_resource_projection_change};

    #[test]
    fn projection_change_decision_table_is_total_and_exact() {
        // Cause/effect graph and constraints match the Kani harness above.
        // This executable table retains every rule at the production boundary:
        // R1 unchanged + any target => retain; R2 changed + any nonempty target
        // => replace; R3 changed + explicit empty target => revoke without a
        // desired installation.
        use LiveResourceProjectionChange::{Replace, Retain, Revoke};

        let rules = [
            (false, false, false, Retain),
            (false, false, true, Retain),
            (false, true, false, Retain),
            (false, true, true, Retain),
            (true, false, false, Revoke),
            (true, false, true, Replace),
            (true, true, false, Replace),
            (true, true, true, Replace),
        ];
        for (changed, inputs, skills, expected) in rules {
            let actual = live_resource_projection_change(changed, inputs, skills);
            assert_eq!(actual, expected);
            assert_eq!(actual.removes_previous(), actual != Retain);
            assert_eq!(actual.installs_desired(), actual == Replace);
        }
    }
}
