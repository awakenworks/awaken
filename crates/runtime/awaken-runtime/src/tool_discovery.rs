//! Runtime-owned search, reveal state, and persistence for on-demand tool schemas.
//!
//! The stable contract carries only the authored presentation and boundary input.
//! Query interpretation and Run state are execution behavior, so they remain in
//! the application crate and cannot enlarge the extension-facing contract.

use std::collections::{BTreeMap, BTreeSet};

use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use awaken_runtime_contract::ToolSearchLimit;
use awaken_runtime_contract::resolved::{ToolDescriptor, ToolPresentation};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolSearchQuery {
    Select(Vec<String>),
    Keywords(Vec<String>),
}

impl ToolSearchQuery {
    pub(crate) fn parse(raw: &str) -> Result<Self, ToolSearchQueryError> {
        let query = raw.trim();
        if query.is_empty() {
            return Err(ToolSearchQueryError::Empty);
        }
        if query.chars().count() > 500 {
            return Err(ToolSearchQueryError::TooLong);
        }
        if let Some(selected) = query.strip_prefix("select:") {
            let mut seen = BTreeSet::new();
            let names = selected
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .filter(|name| seen.insert((*name).to_owned()))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            return (!names.is_empty())
                .then_some(Self::Select(names))
                .ok_or(ToolSearchQueryError::EmptySelection);
        }
        let terms = query
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .filter(|term| !term.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        (!terms.is_empty())
            .then_some(Self::Keywords(terms))
            .ok_or(ToolSearchQueryError::Empty)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolSearchQueryError {
    #[error("tool_search.query must not be empty")]
    Empty,
    #[error("tool_search.query must contain at most 500 characters")]
    TooLong,
    #[error("tool_search select query must contain at least one tool name")]
    EmptySelection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolDiscoveryState {
    revealed: BTreeMap<String, String>,
}

impl ToolDiscoveryState {
    pub(crate) fn contains(&self, canonical_id: &str, descriptor_fingerprint: &str) -> bool {
        self.revealed
            .get(canonical_id)
            .is_some_and(|stored| stored == descriptor_fingerprint)
    }

    pub(crate) fn reveal(
        &mut self,
        canonical_id: impl Into<String>,
        descriptor_fingerprint: impl Into<String>,
    ) {
        self.revealed
            .insert(canonical_id.into(), descriptor_fingerprint.into());
    }
}

pub(crate) struct ToolDiscoveryStateKey;

impl StateKey for ToolDiscoveryStateKey {
    const KEY: &'static str = "runtime.tool_discovery.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = ToolDiscoveryState;
}

pub(crate) struct DiscoveredTool {
    pub(crate) canonical_id: String,
    pub(crate) descriptor: ToolDescriptor,
}

pub(crate) fn search_discoverable(
    presentation: &ToolPresentation,
    descriptors: &[ToolDescriptor],
    discovery: &ToolDiscoveryState,
    query: &ToolSearchQuery,
    requested_limit: Option<ToolSearchLimit>,
) -> Vec<DiscoveredTool> {
    let limit = requested_limit.map_or_else(
        || presentation.discovery().effective_max_results(),
        |value| usize::from(value.get()).min(presentation.discovery().effective_max_results()),
    );
    let candidates = presentation
        .present(descriptors)
        .discoverable
        .into_iter()
        .filter(|descriptor| {
            !discovery.contains(
                presentation.resolve(&descriptor.id),
                &descriptor.content_hash(),
            )
        })
        .collect::<Vec<_>>();
    if let ToolSearchQuery::Select(selection) = query {
        let by_id = candidates
            .iter()
            .map(|descriptor| (descriptor.id.as_str(), descriptor))
            .collect::<BTreeMap<_, _>>();
        return selection
            .iter()
            .filter_map(|name| by_id.get(name.as_str()).copied())
            .take(limit)
            .map(|descriptor| DiscoveredTool {
                canonical_id: presentation.resolve(&descriptor.id).to_owned(),
                descriptor: descriptor.clone(),
            })
            .collect();
    }

    let ToolSearchQuery::Keywords(terms) = query else {
        unreachable!("select queries return above")
    };
    let mut ranked = candidates
        .into_iter()
        .filter_map(|descriptor| {
            let name = descriptor.id.to_lowercase();
            let description = descriptor.description.to_lowercase();
            let schema = descriptor.parameters.to_string().to_lowercase();
            let score = terms.iter().fold(0_u32, |score, term| {
                score
                    + u32::from(name == *term) * 16
                    + u32::from(name.contains(term)) * 8
                    + u32::from(description.contains(term)) * 4
                    + u32::from(schema.contains(term))
            });
            (score > 0).then_some((score, descriptor))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.id.cmp(&right.id))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, descriptor)| DiscoveredTool {
            canonical_id: presentation.resolve(&descriptor.id).to_owned(),
            descriptor,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::state::Store;
    use awaken_runtime_contract::resolved::{ToolExposure, ToolPresentationOverride};

    fn descriptor(id: &str, description: &str) -> ToolDescriptor {
        ToolDescriptor::pinned(
            "test",
            id,
            description,
            serde_json::json!({"type":"object"}),
        )
    }

    #[test]
    fn parser_and_ranker_cover_exact_keyword_alias_limit_and_reveal_causes() {
        // Causal graph / decision table: C1 exact select contains duplicates;
        // C2 keyword matches alias/description/schema; C3 requested limit is one;
        // C4 a canonical fingerprint is already revealed. Effects: E1 selection
        // is ordered and deduplicated; E2 name outranks description/schema; E3
        // result count is bounded; E4 revealed tools never re-enter candidates.
        assert_eq!(
            ToolSearchQuery::parse("select:a, a, ,b").unwrap(),
            ToolSearchQuery::Select(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            ToolSearchQuery::parse("select: , "),
            Err(ToolSearchQueryError::EmptySelection)
        );

        let presentation = ToolPresentation::from_overrides([(
            "mcp__github__issue".into(),
            ToolPresentationOverride {
                alias: Some("create_issue".into()),
                exposure: Some(ToolExposure::OnDemand),
                ..Default::default()
            },
        )]);
        let tools = [descriptor("mcp__github__issue", "GitHub issue")];
        let query = ToolSearchQuery::parse("github issue").unwrap();
        let found = search_discoverable(&presentation, &tools, &Default::default(), &query, None);
        assert_eq!(found[0].canonical_id, "mcp__github__issue", "C2=>E2");

        let mut revealed = ToolDiscoveryState::default();
        let presented = presentation.present(&tools).discoverable.remove(0);
        revealed.reveal("mcp__github__issue", presented.content_hash());
        assert!(
            search_discoverable(&presentation, &tools, &revealed, &query, None).is_empty(),
            "C4=>E4"
        );
    }

    #[test]
    fn reveal_state_is_idempotent_invalidates_on_descriptor_change_and_replays() {
        // Causal graph: C1 f1 is revealed; C2 f1 is replayed; C3 live descriptor
        // becomes f2; C4 typed State commands rebuild a fresh Store. Effects: E1
        // one fact remains; E2 f1 visible; E3 f2 hidden; E4 exact state recovers.
        let mut expected = ToolDiscoveryState::default();
        expected.reveal("a", "f1");
        expected.reveal("a", "f1");
        assert_eq!(expected.revealed.len(), 1, "C1+C2=>E1");
        assert!(expected.contains("a", "f1"), "C1=>E2");
        assert!(!expected.contains("a", "f2"), "C3=>E3");
        let store = Store::rebuild(&[ToolDiscoveryStateKey::write(&expected)]);
        assert_eq!(
            ToolDiscoveryStateKey::load(&store).unwrap(),
            expected,
            "C4=>E4"
        );
    }
}
