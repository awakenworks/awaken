// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The original auxiliary selection law reads one publication source and derives a
// @formal reason: complete instruction override with coherent fingerprints; it owns no store or executor.
//! Select an auxiliary Agent from the Run's already-selected publication source.
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

/// The Memory extractor selected by one already-frozen publication source.
/// Embedded and ordinary Worker extraction share this same selection law.
pub fn memory_extractor_snapshot(
    workspace_id: &str,
    config: awaken_ext_memory::MemoryConfig,
    model: awaken_runtime_contract::resolved::ResolvedModelCandidate,
    publications: &dyn awaken_runtime_contract::PublishedAgentSnapshotSource,
) -> Result<awaken_ext_memory::MemoryExtractorSnapshot, String> {
    let agent_id = config
        .agent_id
        .as_deref()
        .unwrap_or(awaken_ext_memory::MEMORY_AGENT_ID);
    let agent = resolve_auxiliary_snapshot(
        Some(publications),
        workspace_id,
        agent_id,
        awaken_ext_memory::default_memory_agent(
            model,
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        ),
        config.instructions.as_deref(),
    )?;
    Ok(awaken_ext_memory::MemoryExtractorSnapshot {
        agent,
        extraction_prompt: config.extraction_prompt,
    })
}

/// Resolve one auxiliary Agent through the same Coordinator publication source
/// as a foreground Session. The built-in snapshot is only the absent-publication
/// default for the same id; it is not a second mutable catalog. A per-caller
/// instruction override derives one complete snapshot and moves all fingerprint
/// fields together.
pub fn resolve_auxiliary_snapshot(
    publications: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    workspace_id: &str,
    agent_id: &str,
    fallback: ExecutableAgentSnapshot,
    instructions_override: Option<&str>,
) -> Result<ExecutableAgentSnapshot, String> {
    let published = publications.and_then(|source| {
        source.current(
            workspace_id,
            &awaken_runtime_contract::snapshot::AgentId(agent_id.to_string()),
        )
    });
    let mut snapshot = match published {
        Some(snapshot) if snapshot.root_agent_id.0 == agent_id => snapshot,
        Some(snapshot) => {
            return Err(format!(
                "auxiliary Agent `{agent_id}` resolved to publication `{}`",
                snapshot.root_agent_id.0
            ));
        }
        None if fallback.root_agent_id.0 == agent_id => fallback,
        None => {
            return Err(format!(
                "custom auxiliary Agent `{agent_id}` has no published snapshot"
            ));
        }
    };
    if let Some(instructions) = instructions_override.filter(|value| !value.trim().is_empty()) {
        snapshot.resolved_spec.instructions = instructions.to_string();
        snapshot
            .recompute_fingerprint()
            .expect("an executable Agent snapshot is JSON-serializable");
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str, instructions: &str) -> ExecutableAgentSnapshot {
        let mut snapshot = crate::test_support::publication(id);
        snapshot.resolved_spec.instructions = instructions.to_owned();
        snapshot.recompute_fingerprint().unwrap();
        snapshot
    }

    #[test]
    fn auxiliary_resolution_uses_one_publication_source_and_derives_overrides() {
        // Cause/effect decision table: R1 no publication -> built-in snapshot;
        // R2 publication present -> exact published snapshot; R3 R2 + nonblank
        // caller instructions -> complete derived snapshot with a new coherent
        // fingerprint; R4 blank override -> R2 unchanged; R5 custom id without
        // publication -> reject rather than execute the differently-named built-in.
        // R6 a source returning another Agent -> reject even if a default exists.
        struct Publications(ExecutableAgentSnapshot, bool);
        impl awaken_runtime_contract::PublishedAgentSnapshotSource for Publications {
            fn current(
                &self,
                _workspace: &str,
                agent_id: &awaken_runtime_contract::snapshot::AgentId,
            ) -> Option<ExecutableAgentSnapshot> {
                (self.1 || agent_id.0 == self.0.root_agent_id.0).then(|| self.0.clone())
            }

            fn exact(
                &self,
                _workspace: &str,
                _fingerprint: &awaken_runtime_contract::resolved::CatalogFingerprint,
            ) -> Option<ExecutableAgentSnapshot> {
                None
            }

            fn at_revision(
                &self,
                _workspace: &str,
                _agent_id: &awaken_runtime_contract::snapshot::AgentId,
                _source_revision: u64,
            ) -> Option<ExecutableAgentSnapshot> {
                None
            }
        }

        let fallback = config("memory-extractor", "built-in");
        let published = config("memory-extractor", "published");
        assert_eq!(
            resolve_auxiliary_snapshot(None, "ws", "memory-extractor", fallback.clone(), None)
                .expect("the matching built-in is the default")
                .resolved_spec
                .instructions,
            "built-in",
            "R1"
        );
        let source = Publications(published.clone(), false);
        assert_eq!(
            resolve_auxiliary_snapshot(
                Some(&source),
                "ws",
                "memory-extractor",
                fallback.clone(),
                Some(" "),
            )
            .expect("the exact publication exists"),
            published,
            "R2/R4"
        );
        let derived = resolve_auxiliary_snapshot(
            Some(&source),
            "ws",
            "memory-extractor",
            fallback,
            Some("per-agent"),
        )
        .expect("the exact publication exists");
        assert_eq!(derived.resolved_spec.instructions, "per-agent", "R3");
        assert_eq!(
            derived.fingerprint,
            derived.resolved_spec.catalog_fingerprint
        );
        assert_ne!(derived.fingerprint, published.fingerprint, "R3");
        assert!(
            resolve_auxiliary_snapshot(
                None,
                "ws",
                "custom-extractor",
                config("memory-extractor", "built-in"),
                None,
            )
            .is_err(),
            "R5: a custom id cannot silently execute the built-in Agent"
        );
        let misrouted = Publications(config("another-agent", "unrelated"), true);
        assert!(
            resolve_auxiliary_snapshot(
                Some(&misrouted),
                "ws",
                "memory-extractor",
                config("memory-extractor", "built-in"),
                None,
            )
            .is_err(),
            "R6: a mismatched source is refused, never replaced by a default"
        );
    }
}
