//! Canonical aggregate decoding shared by SQLite and PostgreSQL.
//!
//! The aggregate is the sole durable Session model. Indexed SQL columns are
//! projections for lookup and constraints; they are never an alternate source
//! from which a partially specified aggregate can be reconstructed.

use awaken_session_contract::{PersistedSession, SessionRevision};

const CURRENT_AGGREGATE_FORMAT: &str = "awaken.session.v1";

#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct AggregateEnvelope<'a> {
    format: &'static str,
    aggregate: &'a PersistedSession,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedAggregateEnvelope {
    format: String,
    aggregate: PersistedSession,
}

pub(super) fn encode(session: &PersistedSession) -> Result<String, serde_json::Error> {
    verify_aggregate(session)?;
    serde_json::to_string(&AggregateEnvelope {
        format: CURRENT_AGGREGATE_FORMAT,
        aggregate: session,
    })
}

fn invalid(message: impl Into<String>) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

fn verify_aggregate(session: &PersistedSession) -> Result<(), serde_json::Error> {
    session
        .verified_terminal_cleanup()
        .map(|_| ())
        .map_err(|error| {
            invalid(format!(
                "invalid managed Session aggregate terminal cleanup binding: {error}"
            ))
        })
}

fn migrate_legacy_baseline(
    object: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), serde_json::Error> {
    let Some(baseline) = object
        .get_mut("baseline")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Err(invalid("managed Session aggregate has no baseline object"));
    };
    let state = baseline
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("managed Session baseline has no state"))?;
    if state != "frozen" {
        return Err(invalid(format!(
            "unsupported legacy managed Session baseline state `{state}`"
        )));
    }
    // Application contributions were fully consumed into these frozen fields;
    // the nullable receipt was retired and must not become a second authority.
    baseline.remove("application");
    let environment = baseline
        .get_mut("environment")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| invalid("managed Session baseline has no environment snapshot"))?;
    environment
        .entry("self_hosted")
        .or_insert_with(|| serde_json::json!(false));
    let runtime_placement = if environment
        .get("self_hosted")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| invalid("Session self_hosted placement is not boolean"))?
    {
        "worker"
    } else {
        "local"
    };
    environment.entry("idle_retention").or_insert_with(|| {
        serde_json::to_value(awaken_session_contract::EnvironmentIdleRetentionPolicy::default())
            .expect("default idle-retention policy serializes")
    });
    match baseline
        .get("runtime_placement")
        .and_then(serde_json::Value::as_str)
    {
        None | Some("legacy_unspecified") => {
            baseline.insert(
                "runtime_placement".into(),
                serde_json::json!(runtime_placement),
            );
        }
        Some("local" | "worker") => {}
        Some(_) => return Err(invalid("unknown Session runtime placement")),
    }
    Ok(())
}

fn decode_aggregate(data: &str) -> Result<PersistedSession, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("format").is_some() || value.get("aggregate").is_some() {
        let envelope: OwnedAggregateEnvelope = serde_json::from_value(value)?;
        if envelope.format != CURRENT_AGGREGATE_FORMAT {
            return Err(invalid(format!(
                "unsupported managed Session aggregate format `{}`",
                envelope.format
            )));
        }
        return Ok(envelope.aggregate);
    }

    // Pre-envelope aggregates from before the root event-batch convergence did
    // not contain either field. They had no root event-batch provenance to
    // preserve, so the exact migration is two empty collections. Both fields
    // were introduced atomically; a row missing only one is corruption rather
    // than a recognized historical format and remains fail-closed.
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("managed Session aggregate must be an object"))?;
    migrate_legacy_baseline(object)?;

    // Published pre-envelope rows used one lifecycle axis plus an optional
    // archive timestamp. This is the sole historical interpretation; SQL
    // projection columns never participate in aggregate reconstruction.
    if !object.contains_key("disposition") {
        let status = object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid("legacy managed Session has no status"))?
            .to_owned();
        let archived_at = object
            .remove("archived_at")
            .and_then(|value| value.as_str().map(str::to_owned));
        let disposition = match status.as_str() {
            "deleted" => awaken_session_contract::SessionDisposition::Deleted,
            "terminated" if archived_at.is_some() => {
                awaken_session_contract::SessionDisposition::Archived {
                    archived_at: archived_at.expect("checked above"),
                }
            }
            _ => awaken_session_contract::SessionDisposition::Active,
        };
        if status == "deleted" {
            object.insert("status".into(), serde_json::json!("terminated"));
        }
        object.insert(
            "disposition".into(),
            serde_json::to_value(disposition).expect("Session disposition serializes"),
        );
    }

    // The former nullable opaque binding is a recognized predecessor of the
    // typed environment phase. If the typed field exists it is authoritative;
    // carrying both shapes is corruption rather than a merge rule.
    if !object.contains_key("environment") {
        let environment = match object.remove("environment_binding") {
            Some(serde_json::Value::String(binding)) => {
                awaken_session_contract::SessionEnvironmentState::Resident {
                    binding,
                    effect_id: None,
                    generation: None,
                    idle_since_unix_ms: None,
                }
            }
            Some(serde_json::Value::Null) | None => {
                awaken_session_contract::SessionEnvironmentState::Unmaterialized
            }
            Some(_) => {
                return Err(invalid(
                    "legacy Session environment binding is not a string",
                ));
            }
        };
        object.insert(
            "environment".into(),
            serde_json::to_value(environment).expect("Session environment state serializes"),
        );
    } else if object.contains_key("environment_binding") {
        return Err(invalid(
            "managed Session carries both typed and legacy environment state",
        ));
    }

    if !object.contains_key("activity_epoch") {
        let epoch = object
            .remove("activity")
            .and_then(|activity| activity.get("epoch").and_then(serde_json::Value::as_u64))
            .unwrap_or_default();
        object.insert("activity_epoch".into(), serde_json::json!(epoch));
    } else if object.contains_key("activity") {
        return Err(invalid(
            "managed Session carries both scalar and legacy activity state",
        ));
    }

    match (
        object.contains_key("event_batches"),
        object.contains_key("active_activity_epochs"),
    ) {
        (false, false) => {
            object.insert("event_batches".into(), serde_json::json!([]));
            object.insert("active_activity_epochs".into(), serde_json::json!([]));
        }
        (true, true) => {}
        _ => {
            return Err(invalid(
                "managed Session aggregate has a partial event-batch schema",
            ));
        }
    }

    object
        .entry("runtime_active_millis")
        .or_insert_with(|| serde_json::json!(0));
    object.entry("budget").or_insert_with(|| {
        serde_json::to_value(awaken_session_contract::SessionBudgetState::default())
            .expect("default Session budget serializes")
    });
    object.entry("realization_progress").or_insert_with(|| {
        serde_json::to_value(awaken_session_contract::SessionRealizationProgress::default())
            .expect("default Session realization progress serializes")
    });
    object.entry("terminal_cleanup").or_insert_with(|| {
        serde_json::to_value(awaken_session_contract::SessionCleanupOperation::default())
            .expect("default Session cleanup serializes")
    });
    serde_json::from_value(value)
}

pub(super) struct EncodedSessionRow {
    pub aggregate_json: String,
    pub revision: i64,
}

pub(super) fn normalize_published_row(
    stored_session_id: &str,
    aggregate_json: Option<String>,
    revision: i64,
) -> Result<Option<String>, serde_json::Error> {
    let aggregate_json = aggregate_json
        .ok_or_else(|| invalid("published managed Session row has no canonical aggregate"))?;
    let session = decode(EncodedSessionRow {
        aggregate_json: aggregate_json.clone(),
        revision,
    })?;
    if session.session_id != stored_session_id {
        return Err(invalid(
            "managed Session aggregate id does not match its index",
        ));
    }
    let canonical = encode(&session)?;
    Ok((canonical != aggregate_json).then_some(canonical))
}

pub(super) fn decode(row: EncodedSessionRow) -> Result<PersistedSession, serde_json::Error> {
    let revision = SessionRevision(u64::try_from(row.revision).map_err(|_| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Session revision is negative",
        ))
    })?);
    let mut aggregate = decode_aggregate(&row.aggregate_json)?;
    if aggregate.revision != revision {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Session aggregate revision does not match its index",
        )));
    }
    aggregate.resources.compact_settled_activations();
    aggregate.revision = revision;
    verify_aggregate(&aggregate)?;
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    use awaken_session_contract::ManagedSessionRepository as _;
    use awaken_session_contract::resource_plane::{
        BindingId, FileId, InputResourceId, ResourceAccess,
    };
    use awaken_session_contract::{ActivationState, SessionResourceActivation};
    use rusqlite::params;

    use crate::{SqliteManagedSessionRepository, tests::create_fixture, tests::sample};

    fn completed_publication(
        session_id: &str,
        rejected: bool,
    ) -> awaken_session_contract::SessionCleanupOperation {
        let intent: awaken_session_contract::SessionRepositoryPublicationIntent =
            serde_json::from_value(serde_json::json!({
                "input": {
                    "binding_id": "source",
                    "source": {
                        "kind": "repository",
                        "repository_id": "repo-1",
                        "config": {
                            "repository_id": "repo-1",
                            "version": 7,
                            "remote_url": "https://example.test/repo.git"
                        }
                    },
                    "mount_path": "/workspace/source",
                    "access": "read_write"
                },
                "expectation": {
                    "branch": "awf/work",
                    "commit": "0123456789abcdef0123456789abcdef01234567",
                    "expected_prior_commit": "1111111111111111111111111111111111111111"
                }
            }))
            .unwrap();
        let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
        cleanup
            .request_with_publication(session_id, intent)
            .unwrap();
        cleanup.freeze_targets(session_id, [], 3, 5).unwrap();
        let command = cleanup.publication_command(session_id).unwrap().unwrap();
        if rejected {
            let rejection = awaken_session_contract::SessionRepositoryPublicationRejection::new(
                &command,
                awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
            )
            .unwrap();
            cleanup
                .record_repository_publication_rejection(session_id, rejection)
                .unwrap();
        } else {
            let receipt = awaken_session_contract::SessionRepositoryPublicationReceipt::new(
                &command,
                awaken_provisioning_contract::RepositoryPublicationReceipt {
                    repository_id: "repo-1".into(),
                    source_remote_url: "https://example.test/repo.git".into(),
                    branch: command.intent.expectation.branch.clone(),
                    commit: command.intent.expectation.commit.clone(),
                },
            );
            cleanup
                .record_repository_publication_receipt(session_id, receipt)
                .unwrap();
        }
        let root = cleanup.command_for(session_id, session_id).unwrap();
        cleanup
            .record_completion(
                session_id,
                awaken_session_contract::SessionCleanupCompletion::new(&root, Vec::new()),
            )
            .unwrap();
        let receipts = cleanup.recorded_receipts(session_id).unwrap();
        cleanup.complete(session_id, &receipts).unwrap();
        cleanup
    }

    #[tokio::test]
    async fn canonical_aggregate_and_index_revision_are_cross_validated() {
        // Boundary partition: C1 canonical aggregate + matching index -> accept;
        // C2 aggregate/index revision drift -> reject. NOT NULL makes a missing
        // aggregate structurally unrepresentable in the current baseline.
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(&repo, "default", sample("strict-row"), Vec::new()).await;
        assert!(repo.get("strict-row").await.is_ok(), "C1");

        create_fixture(&repo, "default", sample("drifted-row"), Vec::new()).await;
        repo.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE managed_session SET revision = revision + 1 WHERE session_id = ?1",
                params!["drifted-row"],
            )
            .unwrap();
        assert!(repo.get("drifted-row").await.is_err(), "C2");
    }

    /// Persisted normalization cause/effect table: C1 a valid historical row
    /// contains many superseded terminal Resource activations; C2 its root/index
    /// revision still match. Effects: E1 decode retains only the latest terminal
    /// diagnostic generation; E2 normalization emits a canonical replacement;
    /// E3 a second normalization is a no-op. Rule N1=C1+C2=>E1+E2+E3.
    #[test]
    fn historical_terminal_activation_growth_normalizes_once_on_read() {
        let mut session = sample("resource-history-normalization");
        session.resources.revision = 128;
        session.resources.activations = (1..=128)
            .map(|revision| SessionResourceActivation {
                activation_id: format!("activation-{revision}"),
                session_id: session.session_id.clone(),
                revision,
                binding_id: BindingId::from(format!("binding-{revision}")),
                resource_id: InputResourceId::File(FileId::from(format!("file-{revision}"))),
                access: ResourceAccess::ReadOnly,
                state: ActivationState::Released,
                attempts: 1,
                lease_expires_at_unix_ms: None,
                last_error: None,
            })
            .collect();
        let raw = super::encode(&session).expect("N1 historical row");
        let normalized = super::normalize_published_row(
            &session.session_id,
            Some(raw),
            i64::try_from(session.revision.0).unwrap(),
        )
        .expect("N1 normalize")
        .expect("N1/E2 replacement");
        let decoded = super::decode(super::EncodedSessionRow {
            aggregate_json: normalized.clone(),
            revision: i64::try_from(session.revision.0).unwrap(),
        })
        .expect("N1/E1 decode");
        assert_eq!(decoded.resources.activations.len(), 1, "N1/E1");
        assert_eq!(decoded.resources.activations[0].revision, 128, "N1/E1");
        assert!(
            super::normalize_published_row(
                &session.session_id,
                Some(normalized),
                i64::try_from(session.revision.0).unwrap(),
            )
            .expect("N1 renormalize")
            .is_none(),
            "N1/E3"
        );
    }

    #[test]
    fn aggregate_codec_rejects_foreign_completed_publication_outcomes() {
        // Cause/effect matrix: receipt/rejection × encode/decode. Exact outer
        // Session binding passes; rewriting only the outer id leaves a
        // self-consistent intent-local outcome but must fail before storage or
        // normalization can absorb it.
        for (label, rejected) in [("receipt", false), ("rejection", true)] {
            let session_id = format!("codec-{label}");
            let mut session = sample(&session_id);
            session.terminal_cleanup = completed_publication(&session_id, rejected);
            let encoded = super::encode(&session).expect("exact aggregate encodes");

            let foreign_id = format!("foreign-{label}");
            let mut foreign = session.clone();
            foreign.session_id = foreign_id.clone();
            assert!(
                super::encode(&foreign).is_err(),
                "{label}: encode rejects foreign command binding"
            );

            let mut wire: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            wire["aggregate"]["session_id"] = serde_json::json!(foreign_id);
            assert!(
                super::decode(super::EncodedSessionRow {
                    aggregate_json: wire.to_string(),
                    revision: i64::try_from(session.revision.0).unwrap(),
                })
                .is_err(),
                "{label}: decode rejects before canonical normalization"
            );
        }
    }

    #[test]
    fn aggregate_format_decision_table_migrates_only_the_known_legacy_shape() {
        // | Rule | envelope | event_batches | active epochs | Effect |
        // | F1   | v1       | present       | present       | accept |
        // | F2   | absent   | absent        | absent        | migrate empty |
        // | F3   | absent   | present       | absent        | reject |
        // | F4   | unknown  | present       | present       | reject |
        let session = sample("format-matrix");
        let canonical = super::encode(&session).expect("encode v1 envelope");
        assert_eq!(
            super::decode_aggregate(&canonical).expect("F1"),
            session,
            "F1"
        );

        let mut legacy = serde_json::to_value(&session).expect("legacy aggregate JSON");
        legacy.as_object_mut().unwrap().remove("event_batches");
        legacy
            .as_object_mut()
            .unwrap()
            .remove("active_activity_epochs");
        assert_eq!(
            super::decode_aggregate(&legacy.to_string()).expect("F2"),
            session,
            "F2"
        );

        legacy["event_batches"] = serde_json::json!([]);
        assert!(super::decode_aggregate(&legacy.to_string()).is_err(), "F3");

        let mut unknown: serde_json::Value =
            serde_json::from_str(&canonical).expect("canonical envelope JSON");
        unknown["format"] = serde_json::json!("awaken.session.v999");
        assert!(super::decode_aggregate(&unknown.to_string()).is_err(), "F4");
    }

    #[test]
    fn published_pre_envelope_shape_migrates_all_retired_fields_once() {
        // Causes: L1 one-axis lifecycle, L2 nullable environment binding, L3
        // activity object, L4 absent post-publication defaults. Effects: E1
        // independent disposition, E2 typed environment, E3 scalar epoch, E4
        // exact empty/default values. Rule P1=L1+L2+L3+L4=>E1+E2+E3+E4;
        // P2 typed+legacy duplicate state=>fail closed.
        let mut legacy = serde_json::to_value(sample("published-shape")).unwrap();
        let object = legacy.as_object_mut().unwrap();
        for key in [
            "disposition",
            "environment",
            "activity_epoch",
            "event_batches",
            "active_activity_epochs",
            "runtime_active_millis",
            "budget",
            "realization_progress",
            "terminal_cleanup",
        ] {
            object.remove(key);
        }
        object.insert("archived_at".into(), serde_json::Value::Null);
        object.insert(
            "environment_binding".into(),
            serde_json::json!("legacy-binding"),
        );
        object.insert(
            "activity".into(),
            serde_json::json!({"epoch": 41, "state": {"phase": "active"}}),
        );

        let migrated = super::decode_aggregate(&legacy.to_string()).expect("P1");
        assert!(matches!(
            migrated.disposition,
            awaken_session_contract::SessionDisposition::Active
        ));
        assert_eq!(migrated.environment.binding(), Some("legacy-binding"));
        assert_eq!(migrated.activity_epoch, 41);
        assert!(migrated.event_batches.is_empty());
        assert!(migrated.active_activity_epochs.is_empty());
        assert_eq!(migrated.runtime_active_millis, 0);
        assert!(matches!(
            migrated.budget,
            awaken_session_contract::SessionBudgetState::Absent
        ));
        assert!(migrated.terminal_cleanup.is_not_requested());

        let mut duplicate = legacy;
        duplicate["environment"] =
            serde_json::to_value(awaken_session_contract::SessionEnvironmentState::default())
                .unwrap();
        assert!(
            super::decode_aggregate(&duplicate.to_string()).is_err(),
            "P2"
        );
    }
}
