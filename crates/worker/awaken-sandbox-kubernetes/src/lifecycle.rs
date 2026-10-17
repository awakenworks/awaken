use awaken_provisioning_contract::FilesystemContinuity;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, Preconditions};
use kube::{Api, Resource};

use super::error::api_not_found;
use super::{RuntimeError, backend};

/// Observe disappearance of one previously authorized incarnation. A DELETE
/// response only establishes intent; a valid replacement UID or 404 ends this
/// wait without granting any effect on the replacement.
pub(super) async fn await_exact_deletion<K>(
    api: &Api<K>,
    name: &str,
    expected_uid: &str,
    timeout: std::time::Duration,
    description: &str,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + Resource<DynamicType = ()>,
{
    const DELETE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
    let deadline = awaken_time::mono::MonoDeadline::after(timeout);
    loop {
        admit()?;
        let observed = api.get(name).await;
        admit()?;
        let observed = match observed {
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
            Ok(observed) => observed,
        };
        let uid = observed
            .meta()
            .uid
            .as_deref()
            .filter(|uid| !uid.trim().is_empty())
            .ok_or_else(|| backend(format!("{description} `{name}` has no valid UID")))?;
        if uid != expected_uid {
            return Ok(());
        }
        if deadline.has_passed() {
            return Err(backend(format!(
                "{description} `{name}` incarnation `{expected_uid}` was not deleted within {}s",
                timeout.as_secs()
            )));
        }
        admit()?;
        tokio::time::sleep(DELETE_POLL).await;
        admit()?;
    }
}

pub(super) fn termination_grace_period(continuity: FilesystemContinuity) -> Option<i64> {
    matches!(continuity, FilesystemContinuity::Ephemeral).then_some(0)
}

pub(super) fn pod_delete_params(pod: &Pod, preconditions: Preconditions) -> DeleteParams {
    let params = DeleteParams::default().preconditions(preconditions);
    if pod
        .spec
        .as_ref()
        .and_then(|spec| spec.termination_grace_period_seconds)
        == Some(0)
    {
        params.grace_period(0)
    } else {
        params
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::PodSpec;

    use super::*;

    #[test]
    fn filesystem_continuity_controls_only_pod_disposal_grace() {
        /* Pod-disposal cause/effect graph and decision table.
         * Causes: C1 the canonical plan requests ephemeral/retained filesystem
         * continuity; C2 the observed Pod records zero/default termination grace;
         * C3 removal has the exact Pod UID/resourceVersion fence. Effects: E1 an
         * ephemeral Pod and its DELETE request both use grace zero; E2 a retained
         * Session keeps Kubernetes' graceful default; E3 every request preserves
         * the incarnation preconditions. Rules: D1 C1=ephemeral+C2=zero+C3=>E1+E3;
         * D2 C1=retained+C2=default+C3=>E2+E3; D3 a legacy/malformed observation
         * with no Pod spec+C3=>E2+E3. Thus only disposable child/probe environments
         * are reaped inside the bounded provider-disposal window; Session semantics
         * and the existing fail-closed incarnation fence do not change.
         */
        let fence = || Preconditions {
            uid: Some("pod-incarnation-1".into()),
            resource_version: Some("resource-version-1".into()),
        };
        let pod = |continuity| Pod {
            spec: Some(PodSpec {
                termination_grace_period_seconds: termination_grace_period(continuity),
                ..Default::default()
            }),
            ..Default::default()
        };

        let retained = pod(FilesystemContinuity::Retained);
        assert_eq!(
            retained
                .spec
                .as_ref()
                .and_then(|spec| spec.termination_grace_period_seconds),
            None,
            "D2"
        );
        let retained_delete = pod_delete_params(&retained, fence());
        assert_eq!(retained_delete.grace_period_seconds, None, "D2");
        assert_eq!(retained_delete.preconditions, Some(fence()), "D2/E3");

        let ephemeral = pod(FilesystemContinuity::Ephemeral);
        assert_eq!(
            ephemeral
                .spec
                .as_ref()
                .and_then(|spec| spec.termination_grace_period_seconds),
            Some(0),
            "D1"
        );
        let ephemeral_delete = pod_delete_params(&ephemeral, fence());
        assert_eq!(ephemeral_delete.grace_period_seconds, Some(0), "D1");
        assert_eq!(ephemeral_delete.preconditions, Some(fence()), "D1/E3");

        let legacy_delete = pod_delete_params(&Pod::default(), fence());
        assert_eq!(legacy_delete.grace_period_seconds, None, "D3");
        assert_eq!(legacy_delete.preconditions, Some(fence()), "D3/E3");
    }
}
