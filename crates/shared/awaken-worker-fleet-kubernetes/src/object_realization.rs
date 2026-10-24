// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The one create-or-verify fence every Kubernetes realization
// @formal reason: shares: a 409 reuses an object only for its exact digest.
//! Exact realization of one Kubernetes object.
//!
//! A desired object is stamped with the digest of its immutable bytes before
//! its first API write; a retried or concurrent create may reuse an existing
//! same-name object only when that digest matches and it is not terminating.
//! Sandbox Pods, Worker-side package Jobs and their ConfigMaps all realize
//! through this one fence.

use std::fmt::Debug;

use awaken_container_contract::RuntimeError;
use kube::api::PostParams;
use kube::{Api, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{api_conflict, backend};

/// The annotation that carries an object's immutable realization digest.
pub const REALIZATION_DIGEST_ANNOTATION: &str = "awaken.dev/realization-digest";

/// Stamp the exact desired Kubernetes object before its first API write. A retry
/// may reuse an existing object only when this immutable realization fingerprint
/// matches; a same-name object with different bytes/spec fails closed.
pub fn stamp_realization<K>(object: &mut K) -> Result<(), RuntimeError>
where
    K: Resource<DynamicType = ()> + Serialize,
{
    let encoded = serde_json::to_vec(object).map_err(backend)?;
    let digest = blake3::hash(&encoded).to_hex().to_string();
    object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default)
        .insert(REALIZATION_DIGEST_ANNOTATION.into(), digest);
    Ok(())
}

pub fn realization_digest_matches<K>(desired: &K, existing: &K) -> Result<bool, RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    let name = desired.name_any();
    let expected = desired
        .annotations()
        .get(REALIZATION_DIGEST_ANNOTATION)
        .ok_or_else(|| {
            backend(format!(
                "desired {} `{name}` has no realization digest",
                K::kind(&())
            ))
        })?;
    Ok(existing.annotations().get(REALIZATION_DIGEST_ANNOTATION) == Some(expected))
}

/// Create once, or verify and reuse the exact object produced by a concurrent or
/// retried realization. Kubernetes 409 is not success by itself: the immutable
/// fingerprint must match and the existing object must not be terminating.
pub async fn create_or_verify<K>(api: &Api<K>, desired: &K) -> Result<K, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
{
    Ok(create_or_verify_with_status(api, desired).await?.object)
}

pub struct CreateOutcome<K> {
    pub object: K,
    pub created: bool,
}

pub async fn create_or_verify_with_status<K>(
    api: &Api<K>,
    desired: &K,
) -> Result<CreateOutcome<K>, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
{
    match api.create(&PostParams::default(), desired).await {
        Ok(created) => Ok(CreateOutcome {
            object: created,
            created: true,
        }),
        Err(error) if api_conflict(&error) => {
            let name = desired.name_any();
            let existing = api.get(&name).await.map_err(backend)?;
            if !realization_digest_matches(desired, &existing)? {
                return Err(backend(format!(
                    "existing {} `{name}` belongs to a different realization",
                    K::kind(&())
                )));
            }
            if existing.meta().deletion_timestamp.is_some() {
                return Err(backend(format!(
                    "existing {} `{name}` is terminating",
                    K::kind(&())
                )));
            }
            Ok(CreateOutcome {
                object: existing,
                created: false,
            })
        }
        Err(error) => Err(backend(error)),
    }
}

/// Create or reuse one realization and then verify its API-observed immutable
/// projection. The ordinary realization digest remains the first 409 fence;
/// callers with API-defaulted objects add one canonical projection verifier so
/// copying that annotation onto a different spec can never authorize reuse.
pub async fn create_or_verify_exact<K, F>(
    api: &Api<K>,
    desired: &K,
    verify: F,
) -> Result<K, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
    F: FnOnce(&K, &K) -> Result<(), RuntimeError>,
{
    let object = create_or_verify(api, desired).await?;
    verify(desired, &object)?;
    Ok(object)
}

pub async fn create_or_verify_with_status_exact<K, F>(
    api: &Api<K>,
    desired: &K,
    verify: F,
) -> Result<CreateOutcome<K>, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
    F: FnOnce(&K, &K) -> Result<(), RuntimeError>,
{
    let outcome = create_or_verify_with_status(api, desired).await?;
    verify(desired, &outcome.object)?;
    Ok(outcome)
}
