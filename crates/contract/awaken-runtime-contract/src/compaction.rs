//! Neutral committed marker for an exact Run that folded its model context.

use awaken_agent_contract::agent::state::{Action, Command, MergePolicy, Scope, StateCell};

/// Owns the stable `compaction/<run_id>` state encoding shared by the compact
/// plugin and read-side projectors. It is a committed observation, not context
/// reconstruction authority.
pub struct RunCompactionMarker;

impl RunCompactionMarker {
    const KEY_PREFIX: &'static str = "compaction/";

    #[must_use]
    fn key(run_id: &str) -> String {
        format!("{}{run_id}", Self::KEY_PREFIX)
    }

    fn cell(run_id: &str) -> StateCell<bool> {
        StateCell::new(Scope::Thread, MergePolicy::Commutative, Self::key(run_id))
    }

    #[must_use]
    pub fn command(run_id: &str) -> Command {
        Self::cell(run_id).write_bool(true)
    }

    /// Resolve the exact marker from committed command history. The latest
    /// matching command wins, matching [`awaken_agent_contract::agent::state::Store`].
    #[must_use]
    pub fn is_recorded(state: &[Command], run_id: &str) -> bool {
        let key = Self::key(run_id);
        let cell = Self::cell(run_id);
        state
            .iter()
            .rev()
            .find(|command| command.scope == Scope::Thread && command.key.0 == key)
            .is_some_and(|command| match &command.action {
                Action::Set(value) => cell.decode(value) == Ok(true),
                Action::Remove => false,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_run_marker_does_not_leak_between_runs_and_honours_removal() {
        // Causes: C1 Run A has a true marker; C2 Run B has none; C3 a later
        // removal clears A; C4 a present A marker has the wrong durable shape.
        // Effects: E1 only A is initially observed; E2 A is absent after
        // removal; E3 shape drift is not interpreted as a marker. Decision
        // table: R1=C1+!C2=>E1; R2=C1+C3=>E2; R3=C4=>E3.
        // Constraints/invariants: the marker is exact-Run scoped, last-command
        // wins, and only the canonical boolean shape is authoritative.
        let mut commands = vec![RunCompactionMarker::command("run-a")];
        assert!(
            RunCompactionMarker::is_recorded(&commands, "run-a"),
            "R1/E1"
        );
        assert!(
            !RunCompactionMarker::is_recorded(&commands, "run-b"),
            "R1/E1"
        );
        commands.push(Command::remove(
            Scope::Thread,
            MergePolicy::Commutative,
            RunCompactionMarker::key("run-a"),
        ));
        assert!(
            !RunCompactionMarker::is_recorded(&commands, "run-a"),
            "R2/E2"
        );
        commands.push(Command::set(
            Scope::Thread,
            MergePolicy::Commutative,
            RunCompactionMarker::key("run-a"),
            serde_json::Value::String("true".into()),
        ));
        assert!(
            !RunCompactionMarker::is_recorded(&commands, "run-a"),
            "R3/E3"
        );
    }
}
