// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: This read-only walk validates exact Thread, Run and ancestry membership from the
// @formal reason: original committed Native/SEND relationships and rejects ambiguity. It writes no
// @formal reason: registry, grants no new delegation and promises no cross-Thread snapshot.
//! Existing-Thread callback membership from the original committed relationships.
//! No registry is written, and no new delegation is authorized. Unlike the
//! public observed-root projection this walk claims no cross-Thread snapshot.

use super::CommittedReads;
use super::committed_runs::recorded_delegation_registries_from_commands;
use awaken_agent_contract::ThreadId;
use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
use awaken_session_types::{RunError, SessionId};
use std::collections::{HashMap, HashSet};

impl CommittedReads {
    pub async fn task_callback_source(
        &self,
        session_id: &SessionId,
        target: &ThreadId,
        root_agent_id: &str,
    ) -> Result<Option<(String, DelegationOrigin)>, RunError> {
        if target.0.trim().is_empty()
            || target.0 == session_id.as_str()
            || root_agent_id.trim().is_empty()
        {
            return Err(RunError::bad_request(
                "Task child callback requires a distinct logical Thread and frozen root Agent",
            ));
        }
        let root = ThreadId(session_id.to_string());
        let commit = self.commit_for_read(session_id.as_str()).await?;
        let mut pending = vec![root.clone()];
        let mut present = HashSet::new();
        // This disposable read result holds immutable creation edges. It is
        // not a persisted relationship map or a permission/cache authority.
        let mut edges = HashMap::<ThreadId, (ThreadId, String, DelegationOrigin)>::new();
        while let Some(thread) = pending.pop() {
            let Some(run) = commit
                .authoritative_latest_run(&thread)
                .await
                .map_err(RunError::unavailable)?
            else {
                continue;
            };
            if run.thread_id != thread {
                return Err(RunError::unavailable(
                    "Task callback membership answered for another Thread",
                ));
            }
            let snapshot = commit
                .recovery_snapshot(&thread, &run.id)
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?;
            if snapshot.thread_id != thread
                || snapshot
                    .run_record(&run.id)
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                    .is_none()
            {
                return Err(RunError::unavailable(
                    "Task callback prefix has no exact owning Run",
                ));
            }
            present.insert(thread.clone());
            let mut children = super::committed_links::task_callback_coordination_links(
                session_id,
                &snapshot,
                root_agent_id,
            )?
            .into_iter()
            .filter_map(|(link, origin)| {
                link.target
                    .agent_id()
                    .map(|agent| (link.thread_id.clone(), agent.to_owned(), origin))
            })
            .collect::<Vec<_>>();
            for registry in recorded_delegation_registries_from_commands(&snapshot.state)? {
                if snapshot
                    .run_record(registry.parent_run_id())
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                    .is_none()
                {
                    return Err(RunError::unavailable(
                        "Task relationship has no committed parent Run",
                    ));
                }
                for delegation in registry.delegations() {
                    let origin = registry
                        .child_origin(&delegation.id)
                        .map_err(|error| RunError::unavailable(error.to_string()))?;
                    let parent = edges.get(&thread);
                    let parent_agent = parent
                        .map(|(_, agent, _)| agent.as_str())
                        .unwrap_or(root_agent_id);
                    let parent_origin = parent.map(|(_, _, origin)| origin);
                    let expected = DelegationOrigin::nested_for_agent(
                        registry.parent_run_id().clone(),
                        delegation.parent_call_id.clone(),
                        parent_origin.map_or(0, |origin| origin.depth),
                        parent_origin.map_or(&[][..], |origin| origin.agent_lineage.as_slice()),
                        parent_agent.to_owned(),
                    )
                    .map_err(|error| RunError::unavailable(error.to_string()))?;
                    if origin != expected {
                        return Err(RunError::unavailable(
                            "Task callback Native origin differs from its committed ancestry",
                        ));
                    }
                    children.push((
                        ThreadId(delegation.child_run_id.0.clone()),
                        delegation.target_agent_id.clone(),
                        origin,
                    ));
                }
            }
            for (child, agent, origin) in children {
                if child == root || child == thread || agent.trim().is_empty() {
                    return Err(RunError::unavailable(
                        "Task callback relationship is cyclic or incomplete",
                    ));
                }
                let edge = (thread.clone(), agent, origin);
                match edges.entry(child.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(edge);
                        pending.push(child);
                    }
                    std::collections::hash_map::Entry::Occupied(entry) if entry.get() == &edge => {}
                    std::collections::hash_map::Entry::Occupied(_) => {
                        return Err(RunError::unavailable(
                            "Task callback Thread has conflicting committed creation identity",
                        ));
                    }
                }
            }
        }
        Ok(present
            .contains(target)
            .then(|| {
                edges
                    .get(target)
                    .map(|(_, agent, origin)| (agent.clone(), origin.clone()))
            })
            .flatten())
    }
}
