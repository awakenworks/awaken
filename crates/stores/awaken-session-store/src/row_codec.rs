// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Canonical aggregate decoding shared by SQLite and PostgreSQL.
//!
//! The aggregate is the sole durable Session model. Indexed SQL columns are
//! projections for lookup and constraints; they are never an alternate source
//! from which a partially specified aggregate can be reconstructed.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

use awaken_session_contract::PersistedSession;
use awaken_session_types::SessionRevision;

/// v2 carries the complete `McpGeneration` witness: an object of
/// `{attachment_id, version, content_digest}` where v1 stored a bare `u64`.
/// Runtime reads reject old witnesses. Only the operational normalization
/// transaction may upgrade a complete v1 root and advance its writer fence.
const CURRENT_AGGREGATE_FORMAT: &str = "awaken.session.v2";

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
    aggregate: serde_json::Value,
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
    awaken_session_contract::SessionUpdateHistory::validate_for(session)
        .map_err(|error| invalid(error.to_string()))?;
    awaken_session_contract::SessionEnvironmentMaterialization::validate_for(session)
        .map_err(|error| invalid(error.to_string()))?;
    for batch in &session.event_batches {
        batch
            .verify_admission()
            .map_err(|error| invalid(error.to_string()))?;
    }
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
        serde_json::to_value(
            awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy::default(),
        )
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
        return serde_json::from_value(envelope.aggregate);
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
                    archived_at_unix_ms: None,
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
        let legacy_binding = match object.remove("environment_binding") {
            Some(serde_json::Value::String(binding)) => Some(binding),
            Some(serde_json::Value::Null) | None => None,
            Some(_) => {
                return Err(invalid(
                    "legacy Session environment binding is not a string",
                ));
            }
        };
        let environment =
            awaken_session_contract::SessionEnvironmentState::from_legacy_binding(legacy_binding);
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

/// Representation upgrade only: never acknowledge a physical effect, release
/// a lease, or rebuild a receipt. All witness content comes from this one
/// durable root, not from today's configuration or a separate SQL projection.
fn upgrade_v1_root(data: &str, revision: i64) -> Result<Option<String>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some("awaken.session.v1") {
        return Ok(None);
    }
    let mut envelope: OwnedAggregateEnvelope = serde_json::from_value(value)?;
    let aggregate = envelope
        .aggregate
        .as_object_mut()
        .ok_or_else(|| invalid("v1 Session root is not an object"))?;
    if revision < 1
        || aggregate
            .get("revision")
            .and_then(serde_json::Value::as_i64)
            != Some(revision)
    {
        return Err(invalid("v1 Session revision does not match its index"));
    }
    let next_revision = revision
        .checked_add(1)
        .ok_or_else(|| invalid("v1 Session revision exhausted"))?;
    let attachments = aggregate
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("attachments"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| invalid("v1 Session has no MCP attachment array"))?;
    for attachment in attachments {
        let version = attachment
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .filter(|version| *version > 0)
            .ok_or_else(|| invalid("v1 MCP generation is not a positive counter"))?;
        let identity = attachment
            .get("attachment_id")
            .cloned()
            .ok_or_else(|| invalid("v1 MCP attachment has no identity"))?;
        let identity: awaken_session_configuration_contract::mcp::McpAttachmentId =
            serde_json::from_value(identity)?;
        if identity.0.is_empty() {
            return Err(invalid("v1 MCP attachment identity is empty"));
        }
        let draft: awaken_session_configuration_contract::mcp::McpAttachmentDraft =
            serde_json::from_value(attachment.clone())?;
        attachment["generation"] = serde_json::to_value(
            awaken_session_configuration_contract::mcp::McpGeneration::of_draft(
                identity, version, &draft,
            ),
        )?;
    }
    aggregate.insert("revision".into(), serde_json::json!(next_revision));
    // Decode/verify through the canonical current codec below. Anything else
    // that still carries an incompatible witness fails; do not recursively
    // rewrite historical receipts or infer their missing evidence.
    serde_json::to_string(
        &serde_json::json!({"format": CURRENT_AGGREGATE_FORMAT, "aggregate": envelope.aggregate}),
    )
    .map(Some)
}

pub(super) fn normalize_published_row(
    stored_session_id: &str,
    aggregate_json: Option<String>,
    revision: i64,
) -> Result<(PersistedSession, Option<String>), serde_json::Error> {
    let aggregate_json = aggregate_json
        .ok_or_else(|| invalid("published managed Session row has no canonical aggregate"))?;
    let upgraded = upgrade_v1_root(&aggregate_json, revision)?;
    let session = decode(EncodedSessionRow {
        aggregate_json: upgraded.clone().unwrap_or_else(|| aggregate_json.clone()),
        revision: if upgraded.is_some() {
            revision.checked_add(1).expect("upgrade checked capacity")
        } else {
            revision
        },
    })?;
    if session.session_id != stored_session_id {
        return Err(invalid(
            "managed Session aggregate id does not match its index",
        ));
    }
    let canonical = encode(&session)?;
    Ok((session, (canonical != aggregate_json).then_some(canonical)))
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

    use awaken_resource_contract::{BindingId, FileId, InputResourceId, ResourceAccess};
    use awaken_session_contract::SessionRepositoryReads as _;
    use awaken_session_contract::{ActivationState, SessionResourceActivation};
    use awaken_session_types::SessionRevision;

    use crate::SqliteManagedSessionRepository;
    use crate::tests::{close_repository, create_fixture, raw_sqlite, sample};

    #[tokio::test]
    async fn runtime_refuses_v1_while_operational_upgrade_fences_its_writer() {
        // Causes: C1 format is v1, unknown, or current; C2 MCP generation is
        // the historical counter or the complete witness. Effects: E1 reject
        // unsupported formats by name BEFORE interpreting their payload; E2
        // decode a current complete witness; E3 reject a counter mislabeled
        // current. Rules R1=v1/unknown+either=>E1; R2=current+witness=>E2;
        // R3=current+counter=>E3. A real MCP attachment is essential: an empty
        // attachment set cannot expose payload decoding before version checks.
        // C3 cold SQLite startup normalizes roots before recovery quarantine.
        // R4=known v1 counter+C3 => upgrade complete content and advance the
        // root revision exactly once; R5=unknown/mislabeled => preserve bytes.
        // This reproduces the deployed v1/MCP-counter failure through the real
        // repository, not merely serde. No new protocol is introduced, so this
        // is storage/codec evidence, not a physical-stop or migration proof.
        let mut session = crate::tests::sample("format-witness");
        session.mcp = awaken_session_configuration_contract::mcp::SessionMcpAttachmentSet::from_initial(
            vec![awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                name: "fixture".into(),
                target: awaken_agent_contract::McpTarget::parse_http(
                    "https://fixture.test/mcp",
                )
                .unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
        let generation = session.mcp.attachments[0].generation.clone();
        session
            .mcp
            .claim_realization(
                &generation,
                awaken_session_configuration_contract::mcp::McpRealizationClaim {
                    realization_id: "old-realization".into(),
                    runtime_incarnation: "old-worker".into(),
                    lease_epoch: 7,
                    lease_expires_at_unix_ms: 100,
                    stage_idempotency_key: "original-stage".into(),
                },
            )
            .unwrap();
        session
            .mcp
            .activate(&generation, "old-realization")
            .unwrap();
        session
            .mcp
            .acknowledge_publication(&generation, "old-realization")
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join("session-format.db")
            .to_string_lossy()
            .into_owned();
        let repository = SqliteManagedSessionRepository::open(&path).await.unwrap();
        session = create_fixture(&repository, "format-owner", session, Vec::new()).await;
        close_repository(repository).await;
        let current = super::encode(&session).expect("encode current");
        let mut v1: serde_json::Value = serde_json::from_str(&current).expect("reparse");
        for format in ["awaken.session.v1", "awaken.session.v999"] {
            v1["format"] = serde_json::json!(format);
            for generation in [
                serde_json::to_value(&session.mcp.attachments[0].generation).unwrap(),
                serde_json::json!(1),
            ] {
                v1["aggregate"]["mcp"]["attachments"][0]["generation"] = generation;
                let refusal = super::decode_aggregate(&v1.to_string()).expect_err("R1/E1");
                assert!(
                    refusal.to_string().contains(&format!(
                        "unsupported managed Session aggregate format `{format}`"
                    )),
                    "R1/E1 names the format: {refusal}"
                );
                let bytes = v1.to_string();
                let connection = raw_sqlite(&path).await;
                sqlx::query(
                    "UPDATE managed_session SET aggregate_json = ?1, revision = ?3 WHERE session_id = ?2",
                )
                .bind(&bytes)
                .bind(session.session_id.as_str())
                .bind(i64::try_from(session.revision.0).unwrap())
                .execute(&connection)
                .await
                .unwrap();
                connection.close().await;
                if format == "awaken.session.v1"
                    && v1["aggregate"]["mcp"]["attachments"][0]["generation"].is_u64()
                {
                    let mut expected = session.clone();
                    expected.revision.0 += 1;
                    for _ in 0..2 {
                        let repo = SqliteManagedSessionRepository::open(&path)
                            .await
                            .expect("R4 upgrade/reopen");
                        assert_eq!(
                            repo.get(&session.session_id).await.unwrap(),
                            expected,
                            "R4 only representation and writer revision change"
                        );
                    }
                    continue;
                }
                for _ in 0..2 {
                    let error = match SqliteManagedSessionRepository::open(&path).await {
                        Ok(_) => panic!("R4 unsupported cold root must refuse startup"),
                        Err(error) => error,
                    };
                    assert!(
                        if format == "awaken.session.v1" {
                            error.contains("v1 MCP generation is not a positive counter")
                        } else {
                            error.contains(&format!(
                                "unsupported managed Session aggregate format `{format}`"
                            ))
                        },
                        "R5: {error}"
                    );
                    let connection = raw_sqlite(&path).await;
                    let stored: (String, i64) = sqlx::query_as(
                        "SELECT aggregate_json, revision FROM managed_session WHERE session_id = ?1",
                    )
                    .bind(session.session_id.as_str())
                    .fetch_one(&connection)
                    .await
                    .unwrap();
                    connection.close().await;
                    assert_eq!(
                        stored,
                        (bytes.clone(), i64::try_from(session.revision.0).unwrap()),
                        "R4 refusal preserves the original authority on every restart"
                    );
                }
            }
        }
        assert_eq!(super::decode_aggregate(&current).expect("R2/E2"), session);
        let connection = raw_sqlite(&path).await;
        sqlx::query(
            "UPDATE managed_session SET aggregate_json = ?1, revision = ?3 WHERE session_id = ?2",
        )
        .bind(&current)
        .bind(session.session_id.as_str())
        .bind(i64::try_from(session.revision.0).unwrap())
        .execute(&connection)
        .await
        .unwrap();
        connection.close().await;
        let repository = SqliteManagedSessionRepository::open(&path)
            .await
            .expect("R2 current cold root opens");
        assert_eq!(
            repository.get(&session.session_id).await.unwrap(),
            session,
            "R2 current root preserves complete identity through reopen"
        );
        v1["format"] = serde_json::json!(super::CURRENT_AGGREGATE_FORMAT);
        let refusal = super::decode_aggregate(&v1.to_string()).expect_err("R3/E3");
        assert!(
            refusal
                .to_string()
                .contains("expected struct McpGeneration")
        );
    }

    #[test]
    fn v1_upgrade_requires_complete_content_identity_and_revision_capacity() {
        // U1 exact whole-root content + matching positive revision + capacity:
        // derive the same witness, change only root revision/format. U2 each
        // missing/malformed content axis, counter, identity, or revision:
        // refuse. U3 unknown formats remain unsupported. This matrix does not
        // assert that any old process stopped or that remote calls can replay.
        let mut session = sample("upgrade-table");
        session.revision = SessionRevision(1);
        let current = super::encode(&session).unwrap();
        let mut legacy: serde_json::Value = serde_json::from_str(&current).unwrap();
        legacy["format"] = serde_json::json!("awaken.session.v1");
        legacy["aggregate"]["mcp"]["attachments"][0]["generation"] = serde_json::json!(1);
        let (upgraded, _) = super::normalize_published_row(
            session.session_id.as_str(),
            Some(legacy.to_string()),
            1,
        )
        .unwrap();
        let mut expected = session.clone();
        expected.revision = SessionRevision(2);
        assert_eq!(upgraded, expected, "U1 no lifecycle/effect mutation");
        for (pointer, value, index_revision) in [
            (
                "/aggregate/mcp/attachments/0/generation",
                serde_json::json!(0),
                1,
            ),
            (
                "/aggregate/mcp/attachments/0/generation",
                serde_json::json!(-1),
                1,
            ),
            (
                "/aggregate/mcp/attachments/0/attachment_id",
                serde_json::json!(""),
                1,
            ),
            (
                "/aggregate/mcp/attachments/0/target",
                serde_json::Value::Null,
                1,
            ),
            (
                "/aggregate/mcp/attachments/0/origin",
                serde_json::Value::Null,
                1,
            ),
            ("/aggregate/revision", serde_json::json!(2), 1),
            ("/aggregate/revision", serde_json::json!(0), 0),
            ("/aggregate/revision", serde_json::json!(i64::MAX), i64::MAX),
            ("/aggregate/session_id", serde_json::json!("foreign"), 1),
            ("/format", serde_json::json!("awaken.session.v999"), 1),
        ] {
            let mut invalid = legacy.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                super::normalize_published_row(
                    session.session_id.as_str(),
                    Some(invalid.to_string()),
                    index_revision
                )
                .is_err(),
                "U2/U3 {pointer}"
            );
        }
        for field in ["attachment_id", "target", "origin", "generation"] {
            let mut invalid = legacy.clone();
            invalid["aggregate"]["mcp"]["attachments"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                super::normalize_published_row(
                    session.session_id.as_str(),
                    Some(invalid.to_string()),
                    1
                )
                .is_err(),
                "U2 missing {field}"
            );
        }
    }

    fn completed_publication(
        mut session: awaken_session_contract::PersistedSession,
        rejected: bool,
    ) -> awaken_session_contract::PersistedSession {
        let session_id = session.session_id.clone();
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
        session.resources = awaken_session_contract::SessionResourceState::from_active(
            awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                vec![intent.input.clone()],
                Vec::new(),
            )
            .unwrap(),
        );
        session.environment.set_resident("codec-publication-source");
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "codec-worker".into(),
            runtime_incarnation: "codec-worker:boot".into(),
            epoch: 7,
            expires_at_unix_ms: u64::MAX,
        };
        session.realization = Some(lease.clone());
        session
            .archive_with_repository_publication("2026-09-01T00:00:00Z", Some(0), intent)
            .unwrap();
        session.freeze_terminal_cleanup_targets([], 3, 5).unwrap();
        let command = session
            .terminal_cleanup
            .publication_command(&session_id)
            .unwrap()
            .unwrap();
        if rejected {
            let rejection = awaken_session_contract::SessionRepositoryPublicationRejection::new(
                &command,
                awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
            )
            .unwrap();
            session
                .record_repository_publication_rejection(rejection)
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
            session
                .terminal_cleanup
                .record_repository_publication_receipt(&session_id, receipt)
                .unwrap();
        }
        let Some(awaken_session_contract::SessionTerminalCleanupAction::Prepare { mut commands }) =
            session.terminal_cleanup_work_action().unwrap()
        else {
            panic!("publication outcome exposes root preparation");
        };
        assert_eq!(commands.len(), 1, "fixture has only the root target");
        let command = commands.pop().unwrap();
        let effect =
            awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
        let preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap();
        let repository_preparation =
            awaken_session_contract::SessionCleanupRepositoryPreparation::new(
                &session_id,
                "default",
                &session.resources,
            )
            .unwrap();
        session
            .record_terminal_cleanup_preparation(
                "default",
                &lease,
                preparation,
                Some(repository_preparation),
            )
            .unwrap();
        let Some(awaken_session_contract::SessionTerminalCleanupAction::Dispose { command }) =
            session.terminal_cleanup_work_action().unwrap()
        else {
            panic!("durable root preparation exposes physical disposal");
        };
        let receipt = awaken_session_contract::SessionCleanupDisposalReceipt::new(&command);
        session
            .record_terminal_cleanup_disposal("default", &lease, receipt, "codec fixture released")
            .unwrap();
        assert!(
            session.verified_terminal_cleanup().unwrap().is_completed(),
            "disposal receipt completes the aggregate-owned cleanup"
        );
        session
    }

    #[tokio::test]
    async fn canonical_aggregate_and_index_revision_are_cross_validated() {
        // Boundary partition: C1 canonical aggregate + matching index -> accept;
        // C2 aggregate/index revision drift -> reject. NOT NULL makes a missing
        // aggregate structurally unrepresentable in the current baseline.
        let repo = SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap();
        create_fixture(&repo, "default", sample("strict-row"), Vec::new()).await;
        assert!(
            repo.get(&awaken_session_types::SessionId::from("strict-row"))
                .await
                .is_ok(),
            "C1"
        );

        create_fixture(&repo, "default", sample("drifted-row"), Vec::new()).await;
        sqlx::query("UPDATE managed_session SET revision = revision + 1 WHERE session_id = ?1")
            .bind("drifted-row")
            .execute(&repo.pool)
            .await
            .unwrap();
        assert!(
            repo.get(&awaken_session_types::SessionId::from("drifted-row"))
                .await
                .is_err(),
            "C2"
        );
        // I1-I4 cross valid/foreign Agent/foreign model/version0 with history
        // Some/omitted on ACTUAL indexed SQLite rows. Same State identity law
        // must run before history=None can return early. Valid rows roundtrip;
        // malformed rows report Corrupt with aggregate/index/owner byte-stutter.
        // I5 historical initial=None stays absent for both history shapes.
        // Normal tools/MCP authors already run the shared SQL history table;
        // that table retains their valid initial identity through real CAS.
        for history_present in [false, true] {
            for fault in ["valid", "agent", "model", "version", "legacy"] {
                let id = format!("initial-identity-{history_present}-{fault}");
                let root = create_fixture(
                    &repo,
                    "owner",
                    crate::tests::root_update_history::fresh(&id),
                    Vec::new(),
                )
                .await;
                let canonical = super::encode(&root).unwrap();
                let mut wire: serde_json::Value = serde_json::from_str(&canonical).unwrap();
                if !history_present {
                    wire["aggregate"]
                        .as_object_mut()
                        .unwrap()
                        .remove("update_history");
                }
                match fault {
                    "agent" => {
                        wire["aggregate"]["initial_agent_configuration"]["agent_id"] =
                            serde_json::json!("foreign")
                    }
                    "model" => {
                        wire["aggregate"]["initial_agent_configuration"]["model"]["model_id"] =
                            serde_json::json!("foreign")
                    }
                    "version" => {
                        wire["aggregate"]["initial_agent_configuration"]["version"] =
                            serde_json::json!(0)
                    }
                    "legacy" => {
                        wire["aggregate"]
                            .as_object_mut()
                            .unwrap()
                            .remove("initial_agent_configuration");
                    }
                    _ => {}
                }
                let raw = wire.to_string();
                sqlx::query("UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2")
                    .bind(&raw)
                    .bind(&id)
                    .execute(&repo.pool)
                    .await
                    .unwrap();
                let before: (String, i64, String) = sqlx::query_as(
                    "SELECT aggregate_json, revision, scope_id FROM managed_session WHERE session_id = ?1",
                ).bind(&id).fetch_one(&repo.pool).await.unwrap();
                let decoded = repo.get(&root.session_id).await;
                if fault == "valid" || fault == "legacy" {
                    let decoded = decoded.unwrap();
                    assert_eq!(serde_json::to_value(&decoded).unwrap(), wire["aggregate"]);
                    assert_eq!(
                        awaken_session_contract::SessionUpdateHistory::for_session(&decoded)
                            .is_some(),
                        history_present
                    );
                    assert_eq!(
                        awaken_session_contract::SessionUpdateHistory::agent_configuration(
                            &decoded
                        )
                        .is_some(),
                        fault == "valid"
                    );
                } else {
                    assert!(
                        matches!(
                            &decoded,
                            Err(awaken_session_contract::SessionRepositoryError::Corrupt(_))
                        ),
                        "I2-I4 history={history_present} fault={fault}: {decoded:?}"
                    );
                }
                let after: (String, i64, String) = sqlx::query_as(
                    "SELECT aggregate_json, revision, scope_id FROM managed_session WHERE session_id = ?1",
                ).bind(&id).fetch_one(&repo.pool).await.unwrap();
                assert_eq!(
                    after, before,
                    "I1-I5 decode writes nothing or backfills no history"
                );
            }
        }
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
            session.session_id.as_str(),
            Some(raw),
            i64::try_from(session.revision.0).unwrap(),
        )
        .expect("N1 normalize")
        .1
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
                session.session_id.as_str(),
                Some(normalized),
                i64::try_from(session.revision.0).unwrap(),
            )
            .expect("N1 renormalize")
            .1
            .is_none(),
            "N1/E3"
        );
    }

    #[test]
    fn aggregate_codec_rejects_executable_closed_admission_on_read_and_write() {
        // C1 valid closed proof => roundtrip; C2 executable intent injected
        // through an in-memory mutation or stored bytes => reject on encode
        // AND decode. C3 former accepted rows without the new flag remain
        // accepted (covered by the contract's legacy decision table).
        let mut session = sample("codec-admission");
        let closed = awaken_session_contract::SessionEventBatch::closed_admission(
            "closed".into(),
            awaken_session_types::SessionRevision(1),
            "key".into(),
            "fingerprint".into(),
        )
        .unwrap();
        session.event_batches.push(closed);
        let encoded = super::encode(&session).unwrap();
        assert!(
            super::decode(super::EncodedSessionRow {
                aggregate_json: encoded.clone(),
                revision: i64::try_from(session.revision.0).unwrap(),
            })
            .is_ok()
        );
        let accepted = awaken_session_contract::SessionEventBatch::compile(
            &awaken_session_types::SessionId::from("codec-admission"),
            "accepted",
            vec![awaken_session_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "must not run",
                )],
            }],
        )
        .unwrap();
        session.event_batches[0].events = accepted.events;
        assert!(super::encode(&session).is_err(), "C2 write boundary");
        let mut wire: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        wire["aggregate"]["event_batches"][0]["events"] =
            serde_json::to_value(&session.event_batches[0].events).unwrap();
        assert!(
            super::decode(super::EncodedSessionRow {
                aggregate_json: wire.to_string(),
                revision: i64::try_from(session.revision.0).unwrap(),
            })
            .is_err(),
            "C2 read boundary"
        );
    }

    #[test]
    fn aggregate_codec_requires_current_accepted_admission_order_fence() {
        // Cause/effect graph: C1 an accepted batch has an immutable admission
        // fence or no fence; C2 the boundary is encode or decode. Effects: E1
        // fenced rows roundtrip; E2 an in-memory null or stored missing field
        // fails closed. Rules F1=fenced+encode/decode=>E1;
        // F2=null+encode=>E2; F3=missing+decode=>E2. There is no read-time
        // coordinate synthesis or compatibility projection.
        let mut session = sample("codec-admission-fence");
        let mut batch = awaken_session_contract::SessionEventBatch::compile(
            &session.session_id,
            "accepted",
            vec![awaken_session_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "ordered",
                )],
            }],
        )
        .unwrap();
        batch.admitted_revision = awaken_session_types::SessionRevision(1);
        batch
            .anchor_admission_projection(awaken_session_contract::SessionEventProjectionAnchor {
                source_commit_cursor: 0,
            })
            .unwrap();
        session.event_batches.push(batch);
        let encoded = super::encode(&session).expect("F1 encode");
        super::decode(super::EncodedSessionRow {
            aggregate_json: encoded.clone(),
            revision: i64::try_from(session.revision.0).unwrap(),
        })
        .expect("F1 decode");

        session.event_batches[0].admission_projection_anchor = None;
        assert!(super::encode(&session).is_err(), "F2/E2");

        let mut stored: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        stored["aggregate"]["event_batches"][0]
            .as_object_mut()
            .unwrap()
            .remove("admission_projection_anchor");
        assert!(
            super::decode(super::EncodedSessionRow {
                aggregate_json: stored.to_string(),
                revision: i64::try_from(session.revision.0).unwrap(),
            })
            .is_err(),
            "F3/E2"
        );
    }

    #[test]
    fn aggregate_codec_rejects_foreign_completed_publication_outcomes() {
        // Causes: C1 outcome is Published or Rejected; C2 outer Session id is
        // exact or foreign; C3 admission boundary is encode or decode. Effects:
        // E1 exact Completed aggregates encode and decode; E2 foreign bindings
        // fail before storage or normalization. Rules:
        // R1=C1(any)+C2(exact)+C3(encode/decode)->E1;
        // R2=C1(any)+C2(foreign)+C3(encode/decode)->E2. Constraint: each
        // Completed outcome is produced only through the public aggregate
        // request -> freeze -> prepare -> dispose transitions, so rewriting the
        // outer id leaves otherwise self-consistent outcome/disposal receipts.
        for (label, rejected) in [("receipt", false), ("rejection", true)] {
            let session_id = format!("codec-{label}");
            let session = completed_publication(sample(&session_id), rejected);
            let encoded = super::encode(&session).expect("exact aggregate encodes");
            assert_eq!(
                super::decode(super::EncodedSessionRow {
                    aggregate_json: encoded.clone(),
                    revision: i64::try_from(session.revision.0).unwrap(),
                })
                .expect("exact aggregate decodes"),
                session,
                "{label}: exact completed outcome round trips"
            );

            let foreign_id = format!("foreign-{label}");
            let mut foreign = session.clone();
            foreign.session_id = awaken_session_types::SessionId::from(&foreign_id);
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
    fn materialization_wire_validation_reuses_the_root_law_without_inventing_history() {
        // Causes C1 raw old/current envelope; C2 missing/null/present-empty;
        // C3 full valid intent vs foreign Session, non-materialization kind or
        // structurally invalid lease. Effects: old rows stay omitted Unknown;
        // explicit fresh survives; malformed facts fail encode and the actual
        // indexed row decoder (the private JSON shape parser is not admission);
        // historical lease expiry/current-owner changes are not corruption.
        use awaken_session_contract::{
            SessionAggregate, SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind,
        };
        let old = sample("materialization-codec");
        let raw = serde_json::to_string(&old).unwrap();
        assert!(
            super::decode_aggregate(&raw)
                .unwrap()
                .environment_materialization()
                .is_none()
        );
        let mut fresh = awaken_session_contract::PersistedSession::frozen_with_budget(
            old.session_id.clone(),
            old.frozen_baseline().unwrap().clone(),
            old.resources.clone(),
            old.mcp.clone(),
            old.title.clone(),
            old.metadata.clone(),
            old.tools.clone(),
            old.budget.clone(),
        );
        fresh.realization = Some(
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker:1:boot".into(),
                epoch: 1,
                expires_at_unix_ms: 500,
            },
        );
        let fingerprint = fresh
            .frozen_baseline()
            .unwrap()
            .environment
            .config_fingerprint
            .0
            .clone();
        let intent = SessionEnvironmentEffectIntent::new(
            fresh.session_id.clone(),
            SessionEnvironmentEffectKind::Create,
            fresh.realization.clone(),
        )
        .for_environment(fingerprint.clone());
        let mut aggregate = SessionAggregate::try_from(fresh).unwrap();
        aggregate
            .environment()
            .admit_environment_effect("workspace", &intent, 1)
            .unwrap();
        let mut historic = aggregate.into_snapshot();
        historic.realization = None;
        assert_eq!(
            super::decode_aggregate(&super::encode(&historic).unwrap()).unwrap(),
            historic,
            "C3 history independent of current lease"
        );
        for bad in [
            SessionEnvironmentEffectIntent::new(
                "foreign",
                SessionEnvironmentEffectKind::Create,
                intent.realization().cloned(),
            )
            .for_environment(fingerprint.clone()),
            SessionEnvironmentEffectIntent::new(
                historic.session_id.clone(),
                SessionEnvironmentEffectKind::Adopt,
                intent.realization().cloned(),
            )
            .for_environment(fingerprint.clone())
            .from_binding("binding"),
            SessionEnvironmentEffectIntent::new(
                historic.session_id.clone(),
                SessionEnvironmentEffectKind::Create,
                None,
            )
            .for_environment(fingerprint),
        ] {
            let mut invalid = serde_json::to_value(&historic).unwrap();
            invalid["environment_materialization"]["pending"] = serde_json::to_value(bad).unwrap();
            let dto: awaken_session_contract::PersistedSession =
                serde_json::from_value(invalid.clone()).unwrap();
            assert!(super::encode(&dto).is_err(), "C3 encode uses same law");
            for wire in [
                invalid.clone(),
                serde_json::json!({"format": super::CURRENT_AGGREGATE_FORMAT, "aggregate": invalid}),
            ] {
                assert!(
                    super::decode(super::EncodedSessionRow {
                        aggregate_json: wire.to_string(),
                        revision: i64::try_from(historic.revision.0).unwrap(),
                    })
                    .is_err(),
                    "C1/C3 canonical row decoder uses same law"
                );
            }
        }
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
