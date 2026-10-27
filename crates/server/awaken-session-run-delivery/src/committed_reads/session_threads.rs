// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: read-only Thread coordination queries split out of host/coordination.rs.
// @formal reason: Dynamic path: every query reads the parent Session's committed snapshot through the same
// @formal reason: authority's narrower read port, with no cache and no write path, so a child recovery or
// @formal reason: usage read cannot rebuild history or observe a boundary the parent has not committed.
// @formal reason: Evidence: session.multiagent.thread_authority plus this module's own tests
// @formal reason: (thread_usage_reads_authoritative_state_without_rebuilding_history,
// @formal reason: child_recovery_snapshot_reuses_the_parent_commit_consistency_boundary); this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Read-only coordination queries through the parent Session commit boundary.
//!
//! Full recovery still owns cross-region prefixes. State-only queries reuse the
//! same authority's narrower read port; this module has no cache or write path.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource as _};
use awaken_session_types::RunError;

use super::committed_links::apply_lifecycle_latest_runs;
use super::{CommittedReads, CoordinationReadPurpose, project_committed_coordination};
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecycleFeed as _};
use awaken_session_execution_contract::CoordinatedThreadLink;

const MAX_LIFECYCLE_PAGE: usize = 1_024;

impl CommittedReads {
    /// Subscribe through the original hub after the existing committed child
    /// membership check. The root keeps its direct, read-free observation path.
    pub async fn subscribe_session_thread_live(
        &self,
        hub: &crate::ThreadEventHub,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        RunError,
    > {
        if thread_id != session_id {
            let links = self.coordinated_threads(session_id.as_str(), None).await?;
            if !links.iter().any(|link| {
                link.session_id == *session_id && link.thread_id.0.as_str() == thread_id
            }) {
                return Err(RunError::bad_request(
                    "live observer target is not a coordinated Session Thread",
                ));
            }
        }
        Ok(Some(hub.live_subscription(thread_id)?))
    }

    pub async fn coordinated_threads(
        &self,
        session_id: &str,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<CoordinatedThreadLink>, RunError> {
        if observed_root.is_some_and(|snapshot| snapshot.thread_id.0 != session_id) {
            return Err(RunError::internal(
                "coordination query received a snapshot for a different Session root",
            ));
        }
        // Relationship recovery and terminal teardown must remain available
        // after a publication is revoked. The committed advisor call supplies
        // identity; the current publication only enriches its model label.
        let advisor_model = self.advisor_models.advisor_model(session_id)?;
        // One recovery reader and one relationship projector serve both an
        // ordinary latest read and a larger query that already holds this
        // atomic root prefix. Never reopen or clone that supplied prefix.
        let committed = match observed_root {
            Some(snapshot) => std::borrow::Cow::Borrowed(snapshot),
            None => {
                let Some(snapshot) = self
                    .session_thread_recovery_snapshot(session_id, &ThreadId(session_id.into()))
                    .await?
                else {
                    return Ok(Vec::new());
                };
                std::borrow::Cow::Owned(snapshot)
            }
        };
        let mut links = project_committed_coordination(
            &awaken_session_types::SessionId::from(session_id),
            advisor_model.as_deref(),
            &committed,
            CoordinationReadPurpose::Relationships,
        )?
        .links;
        if links.is_empty() {
            return Ok(links);
        }

        // Child Run facts live in their logical Threads and are intentionally
        // absent from the root snapshot. Enrich only from lifecycle facts at or
        // before the snapshot's backend watermark, never from a later prefix.
        let commit = self.commit_for_read(session_id).await?;
        let mut cursor = RunLifecycleCursor::default();
        loop {
            let page = commit
                .events_after(cursor, MAX_LIFECYCLE_PAGE)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            let reached_snapshot_fence =
                apply_lifecycle_latest_runs(&mut links, &page.events, committed.store_cursor);
            if reached_snapshot_fence || page.events.is_empty() || page.next_cursor == cursor {
                break;
            }
            cursor = page.next_cursor;
        }
        Ok(links)
    }

    /// Read the complete logical child prefix from the parent Session's sole
    /// physical commit authority. The existing recovery port owns consistency;
    /// this method only selects its claimed Run from committed child truth.
    pub async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        let commit = self.commit_for_read(session_id).await?;
        let Some(latest_run) = commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(RunError::internal)?
        else {
            return Ok(None);
        };
        commit
            .recovery_snapshot(thread_id, &latest_run.id)
            .await
            .map(Some)
            .map_err(|error| {
                RunError::internal(format!(
                    "recover Session Thread {:?} from parent partition: {error}",
                    thread_id.0
                ))
            })
    }

    /// Read the exact claim-selected logical Run through its parent Session's
    /// physical commit partition. Registered-Worker recovery already holds the
    /// guarded [`RunDispatch`](awaken_dispatch_contract::RunDispatch), so it must not
    /// reopen a logical child as a second physical Session.
    pub async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RunError> {
        self.commit_for_read(session_id)
            .await?
            .recovery_snapshot(thread_id, run_id)
            .await
            .map_err(|error| {
                RunError::internal(format!(
                    "recover Session Thread {:?} Run {:?} from parent partition: {error}",
                    thread_id.0, run_id.0
                ))
            })
    }

    pub async fn session_thread_usage(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        let commit = self.commit_for_read(session_id).await?;
        if commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(RunError::internal)?
            .is_none()
        {
            return Ok(awaken_session_budget_contract::SessionUsage::default());
        }
        // Usage depends only on committed State. Reuse its existing authority
        // query instead of reading every Message, Event, Run and resume ticket
        // again. The parent partition and absent-Run rule stay unchanged; shared
        // stores still bypass their process-local compatibility projections.
        let state = commit
            .committed_state_view(thread_id)
            .await
            .map_err(RunError::internal)?;
        Ok(
            awaken_session_application_contract::session_usage_from_thread_usage(
                awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&state),
            ),
        )
    }

    pub async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        let commit = self.commit_for_read(session_id).await?;
        let state = commit
            .committed_state_view(thread_id)
            .await
            .map_err(RunError::internal)?;
        awaken_agent_contract::thread_disposition_from_committed_state(&state).map_err(|error| {
            RunError::internal(format!(
                "recover disposition for Session Thread {:?}: {error}",
                thread_id.0
            ))
        })
    }
}
