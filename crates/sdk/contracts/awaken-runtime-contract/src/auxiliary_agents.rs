// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The original complete frozen publication closure is shared by all execution callers.
//! Auxiliary Agents a plugin runs on behalf of the Agent that enables it, and
//! the exact non-root publication closure of an admitted Run.
//!
//! A plugin declares which auxiliary Agents its configuration selects; the
//! closure below freezes those publications together with the recursive
//! delegation closure, so a cold Worker or a replay never consults a mutable
//! catalog later. Neither dispatch nor the executor needs to know plugin ids.
use crate::snapshot::{AgentId, ExecutableAgentSnapshot};
use crate::{PublishedAgentSnapshotSource, StaticPublishedAgentSnapshots};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// One auxiliary Agent a plugin's configuration selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuxiliaryAgentSelection {
    /// The Agent the plugin runs.
    pub agent_id: String,
    /// The plugin's built-in default. Default ids have deterministic built-ins
    /// and may be absent from legacy publication bundles; any other id is a
    /// custom Agent that must be frozen from its publication.
    pub default_agent_id: &'static str,
}

impl AuxiliaryAgentSelection {
    /// Whether the selection names a custom Agent rather than the default.
    #[must_use]
    pub fn is_custom(&self) -> bool {
        self.agent_id != self.default_agent_id
    }
}

/// A plugin's declaration of the auxiliary Agents its configuration selects.
pub trait AuxiliaryAgentDeclaration: Send + Sync {
    /// The id an Agent lists in its plugin ids to enable this plugin.
    fn plugin_id(&self) -> &'static str;

    /// The auxiliary Agents `config` selects, or why `config` is invalid
    /// (for example "invalid Memory configuration: ...").
    fn auxiliary_agents(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Vec<AuxiliaryAgentSelection>, String>;
}

/// The publication closure of a Run over the auxiliary Agents its plugins
/// declare. The composition that selects the plugins supplies their
/// declarations; every admission, claim and replay path uses this one value.
#[derive(Clone, Copy)]
pub struct RunPublicationClosure<'a> {
    declarations: &'a [&'a dyn AuxiliaryAgentDeclaration],
}

impl<'a> RunPublicationClosure<'a> {
    #[must_use]
    pub const fn new(declarations: &'a [&'a dyn AuxiliaryAgentDeclaration]) -> Self {
        Self { declarations }
    }

    /// Freeze every non-root Agent publication one Run can execute through:
    /// the recursive delegation closure extended with the auxiliary Agents the
    /// root and its delegates select.
    pub fn freeze(
        &self,
        parent: &ExecutableAgentSnapshot,
        source: Option<&dyn PublishedAgentSnapshotSource>,
        workspace_id: &str,
    ) -> Result<Vec<ExecutableAgentSnapshot>, String> {
        let mut frozen = crate::freeze_delegation_publications(parent, source, workspace_id)
            .map_err(|error| error.to_string())?;
        let owners = std::iter::once(parent)
            .chain(frozen.iter())
            .cloned()
            .collect::<Vec<_>>();
        let owner_ids = owners
            .iter()
            .map(|snapshot| snapshot.root_agent_id.0.clone())
            .collect::<HashSet<_>>();
        let mut seen = std::iter::once(parent)
            .chain(frozen.iter())
            .map(|snapshot| snapshot.fingerprint.clone())
            .collect::<HashSet<_>>();
        // The bool records whether the id was explicitly customized anywhere.
        let mut auxiliary_ids = BTreeMap::<(String, String), bool>::new();
        for owner in &owners {
            for declaration in self.declarations {
                let plugin_id = declaration.plugin_id();
                if !owner
                    .resolved_spec
                    .plugin_ids
                    .iter()
                    .any(|id| id == plugin_id)
                {
                    continue;
                }
                let selections = declaration
                    .auxiliary_agents(owner.resolved_spec.plugin_config.get(plugin_id))
                    .map_err(|reason| format!("Agent `{}` has {reason}", owner.root_agent_id.0))?;
                for selection in selections {
                    let required = selection.is_custom();
                    auxiliary_ids
                        .entry((owner.root_agent_id.0.clone(), selection.agent_id))
                        .and_modify(|existing| *existing |= required)
                        .or_insert(required);
                }
            }
        }
        for ((declaring_owner_id, agent_id), required) in auxiliary_ids {
            if agent_id == declaring_owner_id {
                return Err(format!(
                    "Agent `{declaring_owner_id}` cannot select itself as auxiliary Agent `{agent_id}`"
                ));
            }
            if agent_id == parent.root_agent_id.0 {
                return Err(format!(
                    "auxiliary Agent `{agent_id}` selected by `{declaring_owner_id}` points back to the activation root, which is not part of the non-root publication source"
                ));
            }
            if owner_ids.contains(&agent_id) {
                continue;
            }
            let snapshot =
                source.and_then(|source| source.current(workspace_id, &AgentId(agent_id.clone())));
            let Some(snapshot) = snapshot else {
                if required {
                    return Err(format!(
                        "custom auxiliary Agent `{agent_id}` has no published snapshot"
                    ));
                }
                continue;
            };
            if snapshot.root_agent_id.0 != agent_id {
                return Err(format!(
                    "auxiliary Agent `{agent_id}` resolved to publication `{}`",
                    snapshot.root_agent_id.0
                ));
            }
            if seen.insert(snapshot.fingerprint.clone()) {
                frozen.push(snapshot);
            }
        }
        Ok(frozen)
    }

    /// Reconstruct and validate the exact non-root publication source carried
    /// by one admitted Run. The root remains exclusively in the activation; it
    /// is an input to closure validation, never an entry in this lookup source.
    pub fn exact_source(
        &self,
        root: &ExecutableAgentSnapshot,
        non_root: &[ExecutableAgentSnapshot],
        workspace_id: &str,
    ) -> Result<Arc<StaticPublishedAgentSnapshots>, String> {
        let source = Arc::new(
            StaticPublishedAgentSnapshots::try_new(non_root.iter().cloned())
                .map_err(|error| format!("invalid Agent publication closure: {error}"))?,
        );
        let expected = self.freeze(root, Some(source.as_ref()), workspace_id)?;
        let identities = |snapshots: &[ExecutableAgentSnapshot]| {
            let mut values = snapshots
                .iter()
                .map(|snapshot| {
                    (
                        snapshot.root_agent_id.0.clone(),
                        snapshot.fingerprint.0.clone(),
                    )
                })
                .collect::<Vec<_>>();
            values.sort_unstable();
            values
        };
        if identities(&expected) != identities(non_root) {
            return Err("inexact Agent publication closure".into());
        }
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved::ModelBinding;

    /// A plugin selecting one auxiliary Agent from `{"agent": "<id>"}`;
    /// `{"invalid": true}` is an invalid configuration.
    struct Reviewer;

    impl AuxiliaryAgentDeclaration for Reviewer {
        fn plugin_id(&self) -> &'static str {
            "reviewer"
        }

        fn auxiliary_agents(
            &self,
            config: Option<&serde_json::Value>,
        ) -> Result<Vec<AuxiliaryAgentSelection>, String> {
            if config.is_some_and(|value| value.get("invalid").is_some()) {
                return Err("invalid Reviewer configuration: bad".into());
            }
            Ok(vec![AuxiliaryAgentSelection {
                agent_id: config
                    .and_then(|value| value.get("agent"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("reviewer-default")
                    .to_owned(),
                default_agent_id: "reviewer-default",
            }])
        }
    }

    const CLOSURE: RunPublicationClosure<'static> = RunPublicationClosure::new(&[&Reviewer]);

    fn agent(id: &str, plugin: Option<serde_json::Value>) -> ExecutableAgentSnapshot {
        let builder = ExecutableAgentSnapshot::builder(id)
            .model(ModelBinding::parse("test", "model", "native").expect("fixture model"))
            .fingerprint(format!("{id}-v1"));
        match plugin {
            Some(config) => builder
                .plugins(["reviewer".to_owned()])
                .plugin_config([("reviewer".to_owned(), config)])
                .build(),
            None => builder.build(),
        }
    }

    #[test]
    fn declared_auxiliary_agents_join_the_frozen_publication_closure() {
        // Causes: C1 the owner enables the declaring plugin; C2 its
        // configuration is valid; C3 the selection is the default or a custom
        // id; C4 the publication is present; C5 the selection points at the
        // owner itself. Effects: E1 nothing is frozen; E2 the publication is
        // frozen once; E3 the closure is rejected naming the owner or the id.
        // | Rule | C1 | C2 | C3      | C4 | C5 | Effect |
        // | A1   | n  | -  | -       | -  | -  | E1     |
        // | A2   | y  | y  | default | n  | n  | E1     |
        // | A3   | y  | y  | custom  | y  | n  | E2     |
        // | A4   | y  | y  | custom  | n  | n  | E3 id  |
        // | A5   | y  | n  | -       | -  | -  | E3 owner |
        // | A6   | y  | y  | custom  | -  | y  | E3 id  |
        let custom = agent("custom-reviewer", None);
        let source = StaticPublishedAgentSnapshots::try_new([custom.clone()]).unwrap();
        let empty = StaticPublishedAgentSnapshots::try_new([]).unwrap();

        assert!(
            CLOSURE
                .freeze(&agent("root", None), Some(&source), "w")
                .unwrap()
                .is_empty(),
            "A1"
        );
        let defaults = agent("root", Some(serde_json::json!({})));
        assert!(
            CLOSURE
                .freeze(&defaults, Some(&empty), "w")
                .unwrap()
                .is_empty(),
            "A2"
        );

        let selecting = agent(
            "root",
            Some(serde_json::json!({"agent": "custom-reviewer"})),
        );
        assert_eq!(
            CLOSURE.freeze(&selecting, Some(&source), "w").unwrap(),
            vec![custom.clone()],
            "A3"
        );
        let missing = CLOSURE.freeze(&selecting, Some(&empty), "w").unwrap_err();
        assert!(missing.contains("custom-reviewer"), "A4 {missing}");

        let invalid = agent("root", Some(serde_json::json!({"invalid": true})));
        assert_eq!(
            CLOSURE.freeze(&invalid, None, "w").unwrap_err(),
            "Agent `root` has invalid Reviewer configuration: bad",
            "A5"
        );
        let itself = agent("root", Some(serde_json::json!({"agent": "root"})));
        assert!(
            CLOSURE
                .freeze(&itself, None, "w")
                .unwrap_err()
                .contains("cannot select itself"),
            "A6"
        );

        // The exact source accepts only the closure the root implies.
        assert!(
            CLOSURE
                .exact_source(&selecting, std::slice::from_ref(&custom), "w")
                .is_ok(),
            "X1"
        );
        assert!(CLOSURE.exact_source(&selecting, &[], "w").is_err(), "X2");
    }
}
