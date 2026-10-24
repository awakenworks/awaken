//! The original Session sources a draining Worker is asked to observe.
//!
//! Worker retirement is decided by the Coordinator from Worker-reported data:
//! the Coordinator names each Session whose original source must outlive the
//! Worker's Actor as a [`SourceRetentionSubject`]; the Worker observes that
//! source through its own Sandbox providers and reports a
//! [`awaken_worker_contract::WorkerSourceRetention`] fact for the subject's
//! digest in its heartbeat. The Coordinator holds no provider.

use std::collections::BTreeSet;

use async_trait::async_trait;
use awaken_worker_contract::WorkerSourceRetention;

use crate::{FrozenSessionProjection, RunError, SessionId};

/// The Worker control route that serves a Worker its [`SourceRetentionSubject`]s.
pub const WORKER_SOURCE_RETENTION_SUBJECTS_PATH: &str = "/v1/worker/source-retention/subjects";

/// One original Session source a draining Worker must observe.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRetentionSubject {
    pub session_id: SessionId,
    /// The Session root already authorized its Dispose phase, so the source's
    /// effects are durable and only the original participants must remain.
    pub source_prepared: bool,
    /// The complete frozen projection the observation reads the original
    /// binding, Create fence, Resources and Environment from.
    pub projection: FrozenSessionProjection,
    /// The Coordinator's digest of this exact subject. A reported fact counts
    /// only for an equal digest; any change of the subject makes it stale.
    pub digest: String,
}

impl SourceRetentionSubject {
    /// Name one original source with the digest a reported fact must carry.
    ///
    /// The digest covers the Session, the preparation semantics and every
    /// projection field the observation reads, in a canonical encoding. It
    /// excludes the root revision, which realization lease renewals advance
    /// while the source itself is unchanged.
    pub fn new(
        session_id: SessionId,
        source_prepared: bool,
        projection: FrozenSessionProjection,
    ) -> Result<Self, RunError> {
        let mut value = serde_json::to_value(&projection)
            .map_err(|error| RunError::internal(format!("encode retention subject: {error}")))?;
        if let Some(object) = value.as_object_mut() {
            object.remove("revision");
        }
        let mut canonical = String::new();
        canonical_json(
            &serde_json::json!([session_id.as_str(), source_prepared, value]),
            &mut canonical,
        );
        Ok(Self {
            session_id,
            source_prepared,
            projection,
            digest: blake3::hash(canonical.as_bytes()).to_hex().to_string(),
        })
    }

    /// Whether `reported` says this exact source is retained.
    ///
    /// Decision table (the facts `reported` holds for this subject's Session):
    ///
    /// | Rule | Facts | Digest of those facts | Effect |
    /// |---|---|---|---|
    /// | S1 | none | - | not retained: a missing report fails closed |
    /// | S2 | only for another digest or another Session | stale or foreign | not retained: the subject changed since the Worker observed it |
    /// | S3 | for this digest, every one retained | exact | retained |
    /// | S4 | for this digest, any one not retained | exact | not retained: an unknown, refused or failed observation, and a conflicting report, stay closed |
    #[must_use]
    pub fn reported_retained(&self, reported: &BTreeSet<WorkerSourceRetention>) -> bool {
        let mut facts = reported.iter().filter(|fact| {
            fact.session_id == self.session_id.as_str() && fact.subject == self.digest
        });
        facts.clone().next().is_some() && facts.all(|fact| fact.retained)
    }
}

/// JSON with object keys in sorted order, whatever map the encoder used.
fn canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                canonical_json(&map[key], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The subjects a draining Worker incarnation observes. Implemented by the
/// Coordinator's Worker occupancy read; served to the Worker over its
/// authenticated control channel.
#[async_trait]
pub trait WorkerSourceRetentionSubjects: Send + Sync {
    /// The subjects of the Sessions whose realization lease `worker` holds and
    /// whose original source must be retained across its retirement. Empty
    /// when retirement is already blocked by another fact.
    async fn source_retention_subjects(
        &self,
        worker: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<Vec<SourceRetentionSubject>, RunError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_realization::tests::legacy_resource_projection;

    fn fact(session: &str, subject: &str, retained: bool) -> WorkerSourceRetention {
        WorkerSourceRetention {
            session_id: session.into(),
            subject: subject.into(),
            retained,
        }
    }

    #[test]
    fn a_reported_fact_counts_only_for_the_exact_subject_digest() {
        // Rows S1-S4 of `reported_retained`, then the digest table: D1 a
        // revision-only change (lease renewal) keeps the digest; D2 a changed
        // original binding and D3 changed preparation semantics each produce
        // another digest, so an earlier report is stale.
        let id = SessionId::from("retained-session");
        let subject =
            SourceRetentionSubject::new(id.clone(), false, legacy_resource_projection(true))
                .unwrap();
        let digest = subject.digest.as_str();
        assert!(!subject.reported_retained(&BTreeSet::new()), "S1");
        for (rule, reported) in [
            (
                "S2 stale",
                BTreeSet::from([fact(id.as_str(), "other", true)]),
            ),
            (
                "S2 foreign",
                BTreeSet::from([fact("other-session", digest, true)]),
            ),
            ("S4", BTreeSet::from([fact(id.as_str(), digest, false)])),
            (
                "S4 conflicting",
                BTreeSet::from([
                    fact(id.as_str(), digest, true),
                    fact(id.as_str(), digest, false),
                ]),
            ),
        ] {
            assert!(!subject.reported_retained(&reported), "{rule}");
        }
        assert!(
            subject.reported_retained(&BTreeSet::from([fact(id.as_str(), digest, true)])),
            "S3"
        );
        let mut renewed = subject.projection.clone();
        renewed.revision = crate::SessionRevision(u64::MAX);
        assert_eq!(
            SourceRetentionSubject::new(id.clone(), false, renewed)
                .unwrap()
                .digest,
            subject.digest,
            "D1"
        );
        let mut rebound = subject.projection.clone();
        rebound.environment.set_resident("another-original");
        assert_ne!(
            SourceRetentionSubject::new(id.clone(), false, rebound)
                .unwrap()
                .digest,
            subject.digest,
            "D2"
        );
        assert_ne!(
            SourceRetentionSubject::new(id, true, subject.projection.clone())
                .unwrap()
                .digest,
            subject.digest,
            "D3"
        );
    }
}
