// @formal surface requirement=runtime.run_tool_lifecycle.lifecycle
// @formal reason: Native's first Running and ACP's original Step ThreadCommit record the same Run
// @formal reason: ceiling before Runtime-owned continuation. Recovery and the adjacent table check
// @formal reason: monotone history, exact Run selection, corruption and legacy absence.
//! Original Run ceiling and accepted narrowings in the existing committed log.

use super::{ToolCapabilityNarrowing, ToolPermissionPolicy};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{
    Action, Command, MergePolicy, Scope, StateError, StateKey, Store,
};

/// Original Run input; later accepted inputs may only narrow. Native records
/// before invocation; ACP uses its atomic Step boundary before owned handoff.
/// This input is neither permission to execute nor physical termination proof.
/// Missing legacy evidence stays `None`, never a newly recorded Configured value.
pub struct RunToolCapability;

impl StateKey for RunToolCapability {
    const KEY: &'static str = "runtime.run_tool_capability.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<ToolCapabilityNarrowing>;
}

impl RunToolCapability {
    /// Restore only the already validated Run-owned input, discarding any
    /// projected/unowned value. This changes a disposable view, never the log.
    pub fn project_recorded(store: &mut Store, recorded: Option<ToolCapabilityNarrowing>) {
        store.apply(&match recorded {
            Some(value) => Self::write(&Some(value)),
            None => Self::remove(),
        });
    }

    /// Intersect a current attempt with its validated original input. Legacy
    /// absence removes tool authority; only a fresh activation supplies evidence.
    pub fn effective(
        store: &Store,
        current: ToolCapabilityNarrowing,
        policy: Option<&dyn ToolPermissionPolicy>,
        fresh: bool,
    ) -> Result<ToolCapabilityNarrowing, StateError> {
        let current = current.intersect(
            policy.map_or(ToolCapabilityNarrowing::Configured, |policy| {
                policy.tool_capability_narrowing()
            }),
        );
        Ok(match Self::load(store)? {
            Some(original) => original.intersect(current),
            None if fresh => current,
            None => ToolCapabilityNarrowing::DenyAll,
        })
    }

    /// One recording law for Native and ACP's existing ThreadCommit boundaries.
    /// Legacy absence is never backfilled; accepted changes may only narrow.
    pub fn stage(
        store: &mut Store,
        state: &mut Vec<Command>,
        next: ToolCapabilityNarrowing,
        fresh: bool,
    ) -> Result<(), StateError> {
        if let Some(previous) = Self::load(store)? {
            if previous.intersect(next) != next {
                return Err(StateError {
                    key: Self::KEY.into(),
                    scope: Self::SCOPE,
                    detail: "Run tool capability cannot widen".into(),
                });
            }
            if previous == next {
                return Ok(());
            }
        } else if !fresh {
            return Ok(());
        }
        let command = Self::write(&Some(next));
        store.apply(&command);
        state.push(command);
        Ok(())
    }

    /// Read the original input from the same prefix as execution recovery.
    /// Exact replay is inert; later accepted writes may remove authority, never
    /// restore it or erase the retained original evidence.
    pub fn recorded(
        commands: &[Command],
        run: &RunId,
    ) -> Result<Option<ToolCapabilityNarrowing>, StateError> {
        let invalid = |detail: &str| StateError {
            key: Self::KEY.into(),
            scope: Self::SCOPE,
            detail: detail.into(),
        };
        let mut original = None;
        for command in commands.iter().filter(|command| {
            command.scope == Self::SCOPE
                && command.key == Self::address()
                && command.run_id.as_ref() == Some(run)
        }) {
            if command.merge != Self::MERGE || !matches!(command.action, Action::Set(_)) {
                return Err(invalid(
                    "original Run tool capability was removed or merged",
                ));
            }
            // Reuse the StateKey decoder; there is no second wire codec.
            let next = Self::load(&Store::rebuild(std::slice::from_ref(command)))?
                .ok_or_else(|| invalid("original Run tool capability is null"))?;
            if original
                .is_some_and(|original: ToolCapabilityNarrowing| original.intersect(next) != next)
            {
                return Err(invalid("Run tool capability was widened"));
            }
            original = Some(next);
        }
        Ok(original)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_run_tool_capability_history_is_immutable_and_exactly_scoped() {
        // Causes: C1 original Configured/inherited/DenyAll; C2 matching Run,
        // foreign Run, unowned or Thread data; C3 exact replay, changed value,
        // removal, null, malformed value or changed merge policy; C4 cold serde.
        // R1 exact/replayed own value -> same ceiling; accepted append may only
        // narrow (all 3x3 next-value rules). R2 widening or own C3 damage -> error;
        // R3 foreign/unowned/Thread data -> no authority; R4 no original -> None;
        // R5 cold/warm -> identical outcome and unchanged original command bytes.
        let run = RunId("ceiling-run".into());
        for ceiling in [
            ToolCapabilityNarrowing::Configured,
            ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets,
            ToolCapabilityNarrowing::DenyAll,
        ] {
            let mut original = RunToolCapability::write(&Some(ceiling));
            original.bind_run(&run);
            for next in [
                ToolCapabilityNarrowing::Configured,
                ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets,
                ToolCapabilityNarrowing::DenyAll,
            ] {
                let mut accepted = RunToolCapability::write(&Some(next));
                accepted.bind_run(&run);
                let history = vec![original.clone(), accepted];
                let cold: Vec<Command> =
                    serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
                for view in [&history, &cold] {
                    let result = RunToolCapability::recorded(view, &run);
                    if ceiling.intersect(next) == next {
                        assert_eq!(result.unwrap(), Some(next));
                    } else {
                        assert!(result.is_err());
                    }
                }
                assert_eq!(history, cold, "original retained bytes are unchanged");
            }
            for fault in 0..6 {
                let mut later = original.clone();
                match fault {
                    0 => {}
                    1 => {
                        let changed = if ceiling == ToolCapabilityNarrowing::DenyAll {
                            ToolCapabilityNarrowing::Configured
                        } else {
                            ToolCapabilityNarrowing::DenyAll
                        };
                        later.action = RunToolCapability::write(&Some(changed)).action;
                    }
                    2 => later.action = Action::Remove,
                    3 => later.action = Action::Set(serde_json::Value::Null),
                    4 => later.action = Action::Set(serde_json::json!("unknown-ceiling")),
                    _ => later.merge = MergePolicy::Commutative,
                }
                let commands = vec![original.clone(), later.clone()];
                let bytes = serde_json::to_vec(&commands).unwrap();
                let cold: Vec<Command> = serde_json::from_slice(&bytes).unwrap();
                for view in [&commands, &cold] {
                    let observed = RunToolCapability::recorded(view, &run);
                    if fault == 0 || (fault == 1 && ceiling != ToolCapabilityNarrowing::DenyAll) {
                        assert_eq!(
                            observed.unwrap(),
                            Some(if fault == 0 {
                                ceiling
                            } else {
                                ToolCapabilityNarrowing::DenyAll
                            }),
                            "R1/R5"
                        );
                    } else {
                        assert!(observed.is_err(), "R2/R5 fault={fault}");
                    }
                    assert_eq!(serde_json::to_vec(view).unwrap(), bytes, "R5 inert read");
                }
                for foreign in 0..3 {
                    let mut unrelated = later.clone();
                    match foreign {
                        0 => unrelated.run_id = Some(RunId("other-run".into())),
                        1 => unrelated.run_id = None,
                        _ => unrelated.scope = Scope::Thread,
                    }
                    assert_eq!(
                        RunToolCapability::recorded(&[unrelated], &run).unwrap(),
                        None,
                        "R3 foreign={foreign}"
                    );
                }
            }
        }
        assert_eq!(RunToolCapability::recorded(&[], &run).unwrap(), None, "R4");
    }
}
