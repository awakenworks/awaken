// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The configured Web extension derives eligibility from the existing transcript
// @formal reason: and consumes that read-only view at its single fetch effect boundary. Frozen
// @formal reason: descriptors own source identity; focused tests own provenance and zero-I/O refusal.
//! URL provenance is a derived Web view of the existing committed transcript.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use awaken_agent_contract::agent::{
    content::ContentBlock,
    message::{Message, Role},
    state::{MergePolicy, Scope, Store},
};
use awaken_runtime_contract::plugin::{HookReaction, PhaseContext, PhaseHook, PhaseHookPoint};
#[cfg(test)]
use awaken_tool_contract::tool::ToolError;
#[cfg(test)]
use awaken_web_policy_contract::WebFetchUrlSources;
use awaken_web_policy_contract::WebUrlSourceKind;
use regex::Regex;
use serde::{Deserialize, Serialize};

pub(super) const STATE_KEY: &str = "web_fetch.visible_url_sources";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VisibleUrls {
    format: u32,
    outcome_witnesses: BTreeMap<String, BTreeSet<WebUrlSourceKind>>,
    urls: BTreeMap<WebUrlSourceKind, BTreeMap<String, BTreeSet<String>>>,
    observed_messages: BTreeSet<String>,
}

impl Default for VisibleUrls {
    fn default() -> Self {
        Self {
            format: 3,
            outcome_witnesses: BTreeMap::new(),
            urls: BTreeMap::new(),
            observed_messages: BTreeSet::new(),
        }
    }
}

/// Read-only projection of the Run's already frozen descriptors.
#[derive(Debug, Clone, Default)]
pub(super) struct SourceCatalogue(BTreeMap<String, WebUrlSourceKind>);

impl SourceCatalogue {
    pub(super) fn from_descriptors(
        descriptors: &[awaken_runtime_contract::resolved::ToolDescriptor],
    ) -> Self {
        use awaken_runtime_contract::resolved::ToolKind;
        let mut sources = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for descriptor in descriptors {
            let source = if descriptor.kind == ToolKind::ClientExecuted {
                Some(WebUrlSourceKind::ClientTool)
            } else if descriptor.kind == ToolKind::Regular
                && descriptor.content_namespace() == "builtin"
                && matches!(descriptor.id.as_str(), "web_search" | "web_fetch")
            {
                Some(WebUrlSourceKind::ServerTool)
            } else {
                None
            };
            if !seen.insert(descriptor.id.clone()) {
                ambiguous.insert(descriptor.id.clone());
            }
            if let Some(source) = source {
                sources.insert(descriptor.id.clone(), source);
            }
        }
        for name in ambiguous {
            sources.remove(&name);
        }
        Self(sources)
    }
}

fn canonical_url(raw: &str) -> Option<String> {
    let value = url::Url::parse(raw).ok()?;
    matches!(value.scheme(), "http" | "https").then(|| value.to_string())
}

impl VisibleUrls {
    pub(super) fn load(state: &Store) -> Option<Self> {
        state
            .get(
                Scope::Thread,
                &awaken_agent_contract::agent::state::Key(STATE_KEY.into()),
            )
            .and_then(|value| serde_json::from_value::<Self>(value.clone()).ok())
            .filter(|facts| facts.format == 3)
    }

    fn text(&mut self, source: WebUrlSourceKind, name: &str, text: &str) {
        static URLS: OnceLock<Regex> = OnceLock::new();
        let pattern = URLS.get_or_init(|| {
            Regex::new(r#"(?i:https?)://[^\s<>\"']+"#).expect("constant URL pattern")
        });
        for found in pattern.find_iter(text) {
            let mut raw = found.as_str();
            // Only a known surrounding delimiter can end the URL early.
            // Literal URI punctuation (including IPv6 brackets and query ')')
            // must never authorize a different, silently shortened URL.
            let surrounding = text[..found.start()].chars().next_back();
            if let Some((open, close)) = match surrounding {
                Some('(') => Some(('(', ')')),
                Some('[') => Some(('[', ']')),
                Some('{') => Some(('{', '}')),
                _ => None,
            } {
                let mut depth = 0;
                for (index, ch) in raw.char_indices() {
                    if ch == open {
                        depth += 1;
                    } else if ch == close {
                        if depth == 0 {
                            raw = &raw[..index];
                            break;
                        }
                        depth -= 1;
                    }
                }
            }
            if let Some(url) = canonical_url(raw) {
                self.urls
                    .entry(source)
                    .or_default()
                    .entry(name.to_owned())
                    .or_default()
                    .insert(url);
            }
        }
    }

    fn record_outcome(
        &mut self,
        run_id: &awaken_agent_contract::agent::run::Id,
        catalogue: &SourceCatalogue,
        call: &awaken_tool_contract::tool::ToolCall,
        output: &awaken_tool_contract::tool::ToolOutput,
    ) {
        if output.is_error || call.call_id != output.call_id {
            return;
        }
        if let Some(kind) = catalogue.0.get(&call.tool_id) {
            self.outcome_witnesses
                .entry(outcome_fingerprint(
                    run_id,
                    &call.tool_id,
                    &call.call_id,
                    &call.arguments,
                    &output.content,
                ))
                .or_default()
                .insert(*kind);
        }
    }

    /// Only a matching successful executed outcome can classify a tool result.
    /// This collector never consults the current catalogue to reclassify history.
    fn observe_input(
        &mut self,
        conversation: &[Message],
        input: &[awaken_runtime_contract::llm::ChatMessage],
    ) {
        let mut calls = BTreeMap::new();
        for message in conversation {
            if message.id.is_agent_thread_report() || message.id.is_agent_thread_input() {
                self.observed_messages.insert(message.id.0.clone());
                continue;
            }
            // Context selection/materialization can hide or replace committed
            // data. Refuse ambiguous equal content with no unique message origin.
            let shown = !self.observed_messages.contains(&message.id.0)
                && input
                    .iter()
                    .any(|shown| shown.role == message.role && shown.content == message.content)
                && conversation
                    .iter()
                    .filter(|candidate| {
                        candidate.role == message.role && candidate.content == message.content
                    })
                    .count()
                    == 1;
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, name, input }
                        if message.role == Role::Assistant =>
                    {
                        if let Some((run_id, _, _)) = message.id.assistant_coordinates() {
                            calls.insert(
                                id.clone(),
                                (
                                    awaken_agent_contract::agent::run::Id(run_id.into()),
                                    name.clone(),
                                    input,
                                ),
                            );
                        }
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: false,
                    } if matches!(message.role, Role::User | Role::Tool) => {
                        if let Some((run_id, name, input)) = calls.get(tool_use_id)
                            && shown
                            && !self.observed_messages.contains(&message.id.0)
                        {
                            let fingerprint =
                                outcome_fingerprint(run_id, name, tool_use_id, input, content);
                            if let Some(kinds) = self.outcome_witnesses.get(&fingerprint).cloned() {
                                for kind in kinds {
                                    self.text(
                                        kind,
                                        name,
                                        &awaken_agent_contract::agent::content::extract_text(
                                            content,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    ContentBlock::Text { text } if message.role == Role::User && shown => {
                        self.text(WebUrlSourceKind::UserInput, "", text);
                    }
                    _ => {}
                }
            }
            if shown {
                self.observed_messages.insert(message.id.0.clone());
            }
        }
    }

    pub(super) fn evidence(&self) -> Vec<awaken_web_policy_contract::WebUrlSourceEvidence<'_>> {
        if self.format != 3 {
            return Vec::new();
        }
        self.urls
            .iter()
            .flat_map(|(kind, names)| {
                names.iter().flat_map(move |(name, urls)| {
                    urls.iter().map(
                        move |url| awaken_web_policy_contract::WebUrlSourceEvidence {
                            kind: *kind,
                            name,
                            canonical_url: url,
                        },
                    )
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn permits(&self, raw: &str, policies: &[WebFetchUrlSources]) -> bool {
        awaken_web_policy_contract::admit_fetch_url(raw, &[], policies, Some(&self.evidence()))
            .is_ok()
    }

    fn reaction(self) -> HookReaction {
        HookReaction::state(vec![awaken_agent_contract::agent::state::Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            STATE_KEY,
            serde_json::to_value(self).expect("derived URL sources serialize"),
        )])
    }
}

fn outcome_fingerprint(
    run_id: &awaken_agent_contract::agent::run::Id,
    name: &str,
    call_id: &str,
    arguments: &serde_json::Value,
    content: &[ContentBlock],
) -> String {
    awaken_agent_contract::canonical_json_sha256(
        "web-source-outcome-v3",
        &(run_id, name, call_id, arguments, content),
    )
    .expect("tool outcome correlation values serialize")
}

/// The existing AfterTool receipt binds provenance to the original frozen owner.
/// It records no eligible URLs: promotion waits until the next inference view.
pub(super) struct UrlSourceRecorder(pub Option<Arc<SourceCatalogue>>);

#[async_trait]
impl PhaseHook for UrlSourceRecorder {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::AfterTool
    }
    async fn on_phase(&self, ctx: &PhaseContext, _: &[Message], state: &Store) -> HookReaction {
        let mut visible = VisibleUrls::load(state).unwrap_or_default();
        if let (Some(catalogue), awaken_runtime_contract::plugin::PhaseKind::AfterTool(outcome)) =
            (&self.0, &ctx.kind)
        {
            visible.record_outcome(&ctx.run_id, catalogue, &outcome.call, &outcome.output);
        }
        visible.reaction()
    }
}

pub(super) struct UrlSourceObserver;

#[async_trait]
impl PhaseHook for UrlSourceObserver {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::AfterInference
    }

    async fn on_phase(&self, _: &PhaseContext, _: &[Message], _: &Store) -> HookReaction {
        HookReaction::default()
    }

    async fn on_model_input(
        &self,
        ctx: &PhaseContext,
        input: &[awaken_runtime_contract::llm::ChatMessage],
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        if ctx.kind != awaken_runtime_contract::plugin::PhaseKind::AfterInference {
            return HookReaction::state(vec![]);
        }
        let mut visible = VisibleUrls::load(state).unwrap_or_default();
        visible.observe_input(conversation, input);
        visible.reaction()
    }
}

#[cfg(test)]
fn admit_url(
    url: &str,
    policies: &[WebFetchUrlSources],
    evidence: Option<&VisibleUrls>,
) -> Result<(), ToolError> {
    let facts = evidence.map(VisibleUrls::evidence);
    awaken_web_policy_contract::admit_fetch_url(url, &[], policies, facts.as_deref())
        .map(|_| ())
        .map_err(super::configured_fetch::admission_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::{message::Id as MessageId, run::Id as RunId};
    use awaken_tool_contract::tool::with_tool_state_context;
    use serde_json::json;

    fn observed_input(
        conversation: &[Message],
    ) -> Arc<[awaken_runtime_contract::llm::ChatMessage]> {
        conversation
            .iter()
            .map(|message| awaken_runtime_contract::llm::ChatMessage {
                role: message.role,
                content: message.content.clone(),
            })
            .collect::<Vec<_>>()
            .into()
    }

    #[tokio::test]
    async fn reused_call_coordinates_in_another_run_cannot_transfer_source_ownership() {
        // W1 a server outcome is executed but not shown; W2 a later Run's
        // same-name client repeats identical call id/args/content; W3 only that
        // client's committed result reaches inference. The old witness must
        // not become server provenance for W3, including after view recovery.
        use awaken_runtime_contract::plugin::{AfterToolContext, PhaseKind};
        use awaken_tool_contract::tool::{ToolCall, ToolOutput};
        let url = "https://example.com/reused";
        let call = ToolCall {
            call_id: "same-call".into(),
            tool_id: "web_search".into(),
            arguments: json!({"query":"same"}),
        };
        let output = ToolOutput::ok(&call.call_id, url);
        let mut state = Store::new();
        for (run_id, clients) in [("server-run", vec![]), ("client-run", vec!["web_search"])] {
            let context = PhaseContext {
                run_id: RunId(run_id.into()),
                step: 0,
                kind: PhaseKind::AfterTool(AfterToolContext {
                    call: call.clone(),
                    output: output.clone(),
                }),
            };
            for command in UrlSourceRecorder(Some(Arc::new(catalogue(&clients))))
                .on_phase(&context, &[], &state)
                .await
                .state
            {
                state.apply(&command);
            }
        }
        let recovered: VisibleUrls = serde_json::from_value(
            serde_json::to_value(VisibleUrls::load(&state).unwrap()).unwrap(),
        )
        .unwrap();
        state = Store::new();
        for command in recovered.reaction().state {
            state.apply(&command);
        }
        let transcript = vec![
            Message::new(
                MessageId::assistant(&RunId("client-run".into()), 0),
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: call.call_id.clone(),
                    name: call.tool_id.clone(),
                    input: call.arguments.clone(),
                }],
            ),
            Message::new(
                MessageId::tool_result(&call.call_id),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: call.call_id,
                    content: output.content,
                    is_error: false,
                }],
            ),
        ];
        let before = PhaseContext {
            run_id: RunId("client-run".into()),
            step: 1,
            kind: PhaseKind::AfterInference,
        };
        for command in UrlSourceObserver
            .on_model_input(&before, &observed_input(&transcript), &transcript, &state)
            .await
            .state
        {
            state.apply(&command);
        }
        let facts = VisibleUrls::load(&state).unwrap();
        let server =
            serde_json::from_value(json!({"client_tool_results":"none","user_input":"none"}))
                .unwrap();
        let client =
            serde_json::from_value(json!({"server_tool_results":"none","user_input":"none"}))
                .unwrap();
        assert!(!facts.permits(url, &[server]));
        assert!(facts.permits(url, &[client]));
    }

    #[test]
    fn model_authored_child_and_advisor_seeds_are_never_user_url_evidence() {
        // I1 ordinary committed user text -> user evidence; I2 delegated task
        // and advisor inputs use User role only for model transport -> none.
        // Their existing canonical identities survive a cold view rebuild.
        let child = awaken_agent_contract::agent::delegation::DelegationOrigin::root(
            RunId("parent".into()),
            "call",
        )
        .child_run_id();
        let url = "https://example.com/model-invented";
        let user_only = serde_json::from_value(
            json!({"client_tool_results":"none","server_tool_results":"none"}),
        )
        .unwrap();
        for id in [
            MessageId::agent_thread_input(&child),
            MessageId::agent_advisor_input(&child),
        ] {
            let mut facts = VisibleUrls::default();
            facts.observe_committed(&[Message::text(id, Role::User, url)]);
            assert!(!facts.permits(url, std::slice::from_ref(&user_only)));
            assert!(super::admit_url(url, std::slice::from_ref(&user_only), Some(&facts)).is_err());
        }
        let mut facts = VisibleUrls::default();
        facts.observe_committed(&[Message::text(MessageId("human".into()), Role::User, url)]);
        assert!(facts.permits(url, &[user_only]));
    }

    fn admit_url(url: &str, policies: &[WebFetchUrlSources]) -> Result<(), ToolError> {
        let evidence = awaken_tool_contract::tool::current_tool_state()
            .and_then(|state| VisibleUrls::load(&state));
        super::admit_url(url, policies, evidence.as_ref())
    }
    fn catalogue(clients: &[&str]) -> SourceCatalogue {
        let mut descriptors = vec![
            super::super::web_search_descriptor(),
            super::super::web_fetch_descriptor(),
        ];
        descriptors.retain(|descriptor| !clients.contains(&descriptor.id.as_str()));
        descriptors.extend(clients.iter().map(|name| {
            awaken_runtime_contract::resolved::ToolDescriptor::client_executed(
                *name,
                "client",
                json!({"type":"object"}),
            )
        }));
        SourceCatalogue::from_descriptors(&descriptors)
    }

    #[tokio::test]
    async fn successful_outcome_is_required_and_promoted_only_at_inference() {
        // R1 transcript/name without an executed witness -> no source; R2
        // successful AfterTool -> still no URL before the next inference;
        // R3 exact call/arguments/content match -> source with original owner;
        // R4 changed arguments/content, error, missing or unsupported cache ->
        // reject. A restored client witness cannot become a server witness.
        use awaken_runtime_contract::plugin::{AfterToolContext, PhaseKind};
        use awaken_tool_contract::tool::{ToolCall, ToolOutput};
        let url = "https://example.com/receipt";
        let call = ToolCall {
            call_id: "call".into(),
            tool_id: "web_search".into(),
            arguments: json!({"query":"query"}),
        };
        let output = ToolOutput {
            call_id: call.call_id.clone(),
            content: vec![ContentBlock::text(url)],
            is_error: false,
            state: vec![],
        };
        let transcript = vec![
            Message::new(
                MessageId::assistant(&RunId("r".into()), 0),
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: call.call_id.clone(),
                    name: call.tool_id.clone(),
                    input: call.arguments.clone(),
                }],
            ),
            Message::new(
                MessageId("result".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: call.call_id.clone(),
                    content: output.content.clone(),
                    is_error: false,
                }],
            ),
        ];
        let mut state = Store::new();
        let before = PhaseContext {
            run_id: RunId("r".into()),
            step: 1,
            kind: PhaseKind::AfterInference,
        };
        let server: WebFetchUrlSources =
            serde_json::from_value(json!({"client_tool_results":"none", "user_input":"none"}))
                .unwrap();
        let client: WebFetchUrlSources =
            serde_json::from_value(json!({"server_tool_results":"none", "user_input":"none"}))
                .unwrap();
        let reaction = UrlSourceObserver
            .on_model_input(&before, &observed_input(&transcript), &transcript, &state)
            .await;
        for command in reaction.state {
            state.apply(&command);
        }
        assert!(
            !VisibleUrls::load(&state)
                .unwrap()
                .permits(url, std::slice::from_ref(&server))
        );
        // Use a clean view: a missing historical witness stays missing, whereas
        // an actual new successful outcome can contribute at its next inference.
        state = Store::new();
        let after = PhaseContext {
            run_id: RunId("r".into()),
            step: 1,
            kind: PhaseKind::AfterTool(AfterToolContext {
                call: call.clone(),
                output: output.clone(),
            }),
        };
        let recorder = UrlSourceRecorder(Some(Arc::new(catalogue(&["web_search"]))));
        for command in recorder.on_phase(&after, &[], &state).await.state {
            state.apply(&command);
        }
        assert!(
            !VisibleUrls::load(&state)
                .unwrap()
                .permits(url, std::slice::from_ref(&client))
        );
        for command in UrlSourceObserver
            .on_model_input(&before, &observed_input(&transcript), &transcript, &state)
            .await
            .state
        {
            state.apply(&command);
        }
        let restored = VisibleUrls::load(&state).unwrap();
        assert!(restored.permits(url, std::slice::from_ref(&client)));
        assert!(!restored.permits(url, std::slice::from_ref(&server)));
        for (content, arguments, is_error) in [
            (
                vec![ContentBlock::text("https://example.com/changed")],
                call.arguments.clone(),
                false,
            ),
            (output.content.clone(), json!({"query":"changed"}), false),
            (output.content.clone(), call.arguments.clone(), true),
        ] {
            let mut facts = VisibleUrls::default();
            facts.record_outcome(
                &RunId("r".into()),
                &catalogue(&[]),
                &call,
                &ToolOutput {
                    content,
                    is_error,
                    ..output.clone()
                },
            );
            let mut changed = transcript.clone();
            changed[0].content = vec![ContentBlock::ToolUse {
                id: call.call_id.clone(),
                name: call.tool_id.clone(),
                input: arguments,
            }];
            facts.observe_committed(&changed);
            assert!(!facts.permits(url, std::slice::from_ref(&server)));
        }
        // A version fence invalidates both receipts and eligible URLs. The
        // transcript cannot reconstruct tool ownership from a current name.
        let mut unsupported = serde_json::to_value(restored).unwrap();
        unsupported["format"] = json!(99);
        let mut damaged = Store::new();
        damaged.apply(&awaken_agent_contract::agent::state::Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            STATE_KEY,
            unsupported,
        ));
        for command in UrlSourceObserver
            .on_model_input(&before, &observed_input(&transcript), &transcript, &damaged)
            .await
            .state
        {
            damaged.apply(&command);
        }
        assert!(
            !VisibleUrls::load(&damaged)
                .unwrap()
                .permits(url, &[client, server])
        );
    }

    impl VisibleUrls {
        fn observe_committed(&mut self, conversation: &[Message]) {
            self.observe_input(conversation, &observed_input(conversation));
        }
        fn observe(&mut self, conversation: &[Message], catalogue: &SourceCatalogue) {
            use awaken_tool_contract::tool::{ToolCall, ToolOutput};
            let mut calls = BTreeMap::new();
            for message in conversation {
                for block in &message.content {
                    match block {
                        ContentBlock::ToolUse { id, name, input }
                            if message.role == Role::Assistant =>
                        {
                            if let Some((run_id, _, _)) = message.id.assistant_coordinates() {
                                calls.insert(
                                    id.clone(),
                                    (
                                        RunId(run_id.into()),
                                        ToolCall {
                                            call_id: id.clone(),
                                            tool_id: name.clone(),
                                            arguments: input.clone(),
                                        },
                                    ),
                                );
                            }
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } if !self.observed_messages.contains(&message.id.0) => {
                            if let Some((run_id, call)) = calls.get(tool_use_id) {
                                self.record_outcome(
                                    run_id,
                                    catalogue,
                                    call,
                                    &ToolOutput {
                                        call_id: tool_use_id.clone(),
                                        content: content.clone(),
                                        is_error: *is_error,
                                        state: vec![],
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            self.observe_committed(conversation);
        }
    }

    #[test]
    fn source_evidence_requires_descriptor_ownership_and_supported_format() {
        // R1 exact builtin descriptor -> server; R2 same name under another
        // owner or absent descriptor -> no source; R3 duplicate identity ->
        // fail closed; R4 unknown/missing fact format -> no admissible evidence.
        use awaken_runtime_contract::resolved::ToolDescriptor;
        let builtin = super::super::web_search_descriptor();
        let impostor = ToolDescriptor::pinned(
            "mcp:foreign",
            "web_search",
            "search",
            json!({"type":"object"}),
        );
        for (descriptors, allowed) in [
            (vec![builtin.clone()], true),
            (vec![impostor.clone()], false),
            (vec![], false),
            (vec![impostor.clone(), builtin.clone()], false),
            (vec![builtin, impostor], false),
        ] {
            let catalogue = SourceCatalogue::from_descriptors(&descriptors);
            assert_eq!(
                catalogue.0.get("web_search") == Some(&WebUrlSourceKind::ServerTool),
                allowed
            );
        }
        let mut facts = VisibleUrls::default();
        let url = "https://example.com/source";
        facts.text(WebUrlSourceKind::UserInput, "", url);
        let policies = [WebFetchUrlSources::default()];
        assert!(super::admit_url(url, &policies, Some(&facts)).is_ok());
        facts.format = 99;
        assert!(super::admit_url(url, &policies, Some(&facts)).is_err());
        let mut state = Store::new();
        state.apply(&awaken_agent_contract::agent::state::Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            STATE_KEY,
            serde_json::to_value(&facts).unwrap(),
        ));
        assert!(VisibleUrls::load(&state).is_none());
        let mut legacy = serde_json::to_value(facts).unwrap();
        legacy.as_object_mut().unwrap().remove("format");
        assert!(serde_json::from_value::<VisibleUrls>(legacy).is_err());
    }

    #[tokio::test]
    async fn visible_url_source_admission_decision_table() {
        // C1 configured source policy; C2 URL shown in user/tool result; C3 exact
        // allowed tool name; C4 assistant text/arguments, unmatched result or
        // inter-thread report; C5 missing/recovered derived state; C6 inherited
        // narrowing. R1 C2+C3 -> allow; R2 !C2/C4/C5(missing) -> block before
        // the same fetch I/O boundary; R3 recovered State -> same result; R4 every C6 must allow.
        // Tools in one inference cannot authorize URLs not yet shown back to
        // the model: only the observed model input promotes matching shown messages.
        let url = "https://example.com/visible";
        let policy: WebFetchUrlSources = serde_json::from_value(json!({
            "user_input":"none", "server_tool_results":{"type":"only", "tools":[{"type":"tool_reference", "name":"web_search"}]}, "client_tool_results":"none"
        })).unwrap();
        let mut state = Store::new();
        assert!(admit_url(url, std::slice::from_ref(&policy)).is_err());
        let conversation = vec![
            Message::text(MessageId("model".into()), Role::Assistant, url),
            Message::new(
                MessageId::assistant(&RunId("r".into()), 0),
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "s1".into(),
                    name: "web_search".into(),
                    input: json!({"model_url":url}),
                }],
            ),
            Message::new(
                MessageId("result".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "s1".into(),
                    content: vec![ContentBlock::text(url)],
                    is_error: false,
                }],
            ),
        ];
        let ctx = PhaseContext {
            run_id: RunId("r".into()),
            step: 1,
            kind: awaken_runtime_contract::plugin::PhaseKind::AfterInference,
        };
        let mut recorded = VisibleUrls::default();
        recorded.observe(&conversation, &catalogue(&[]));
        recorded.urls.clear();
        recorded.observed_messages.clear();
        state.apply(&awaken_agent_contract::agent::state::Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            STATE_KEY,
            serde_json::to_value(recorded).unwrap(),
        ));
        for command in UrlSourceObserver
            .on_model_input(&ctx, &observed_input(&conversation), &conversation, &state)
            .await
            .state
        {
            state.apply(&command);
        }
        with_tool_state_context(state.clone(), async {
            assert!(admit_url(url, std::slice::from_ref(&policy)).is_ok());
            assert!(
                admit_url("https://example.com/unseen", std::slice::from_ref(&policy)).is_err()
            );
        })
        .await;
        let stored = serde_json::to_value(VisibleUrls::load(&state).unwrap()).unwrap();
        let recovered: VisibleUrls = serde_json::from_value(stored).unwrap();
        assert!(recovered.permits(url, &[policy]));
        let deny_user: WebFetchUrlSources = serde_json::from_value(
            json!({"server_tool_results":"none", "client_tool_results":"none"}),
        )
        .unwrap();
        assert!(!recovered.permits(url, &[deny_user]));
        for message in [
            Message::text(MessageId("m".into()), Role::Assistant, url),
            Message::text(
                MessageId::agent_thread_report(&RunId("child".into())),
                Role::User,
                url,
            ),
            Message::new(
                MessageId("missing".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "unknown".into(),
                    content: vec![ContentBlock::text(url)],
                    is_error: false,
                }],
            ),
        ] {
            let mut facts = VisibleUrls::default();
            facts.observe(&[message], &catalogue(&[]));
            assert!(!facts.permits(url, &[WebFetchUrlSources::default()]));
        }
        let mut user = VisibleUrls::default();
        user.observe(
            &[Message::text(
                MessageId("u".into()),
                Role::User,
                "See <HTTPS://EXAMPLE.COM/visible>.",
            )],
            &catalogue(&[]),
        );
        assert!(user.permits(url, &[WebFetchUrlSources::default()]));
        assert!(!user.permits(
            "https://example.com/visible-extra",
            &[WebFetchUrlSources::default()]
        ));
    }

    #[tokio::test]
    async fn source_identity_uses_frozen_client_descriptors_and_survives_recovery() {
        // C1 same name as a built-in; C2 frozen client descriptor present;
        // C3 source-specific allow/deny; C4 a later catalogue changes;
        // C5 missing catalogue or a non-custom, non-Web tool.
        // R1 C1+C2 -> client only, never server; R2 C1+!C2 -> server;
        // R3 a restored observation retains its original class under C4;
        // R4 C5 contributes no tool-result URLs. Classification precedes every
        // policy so one allowed source cannot impersonate another source.
        let url = "https://example.com/tool-result";
        let transcript = |name: &str| {
            vec![
                Message::new(
                    MessageId::assistant(&RunId("r".into()), 0),
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: name.into(),
                        input: json!({}),
                    }],
                ),
                Message::new(
                    MessageId("result".into()),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: vec![ContentBlock::text(url)],
                        is_error: false,
                    }],
                ),
            ]
        };
        let server: WebFetchUrlSources = serde_json::from_value(json!({
            "client_tool_results":"none", "user_input":"none"
        }))
        .unwrap();
        let client: WebFetchUrlSources = serde_json::from_value(json!({
            "server_tool_results":"none", "user_input":"none"
        }))
        .unwrap();
        let mut view = VisibleUrls::default();
        view.observe(&transcript("web_search"), &catalogue(&["web_search"]));
        assert!(view.permits(url, std::slice::from_ref(&client)));
        assert!(!view.permits(url, std::slice::from_ref(&server)));
        let mut recovered: VisibleUrls =
            serde_json::from_value(serde_json::to_value(view).unwrap()).unwrap();
        recovered.observe(&transcript("web_search"), &catalogue(&[]));
        assert!(!recovered.permits(url, std::slice::from_ref(&server)));
        assert!(recovered.permits(url, std::slice::from_ref(&client)));
        let mut server_view = VisibleUrls::default();
        server_view.observe(&transcript("web_search"), &catalogue(&[]));
        assert!(server_view.permits(url, std::slice::from_ref(&server)));
        assert!(!server_view.permits(url, std::slice::from_ref(&client)));
        assert!(!server_view.permits(url, &[server.clone(), client]));
        let mut other_tool = VisibleUrls::default();
        other_tool.observe(&transcript("bash"), &catalogue(&[]));
        assert!(!other_tool.permits(url, &[WebFetchUrlSources::default()]));
        let ctx = PhaseContext {
            run_id: RunId("missing-catalogue".into()),
            step: 1,
            kind: awaken_runtime_contract::plugin::PhaseKind::AfterInference,
        };
        let mut state = Store::new();
        for command in UrlSourceObserver
            .on_model_input(
                &ctx,
                &observed_input(&transcript("web_search")),
                &transcript("web_search"),
                &state,
            )
            .await
            .state
        {
            state.apply(&command);
        }
        assert!(!VisibleUrls::load(&state).unwrap().permits(url, &[server]));
    }

    #[test]
    fn visible_urls_preserve_complete_uri_syntax() {
        // U1 literal IPv6 authority; U2 balanced path parentheses; U3 an
        // explicit Markdown/prose wrapper plus sentence punctuation; U4 bare
        // query/punctuation is part of the literal URI. R1 register the whole
        // displayed URI, R2 never register the former trimmed variant. Known
        // wrappers own their closing delimiter; no alternate URL is guessed.
        for (text, original, altered) in [
            ("http://[::1]", "http://[::1]", "http://[::1"),
            (
                "<https://[2001:db8::1]>",
                "https://[2001:db8::1]",
                "https://[2001:db8::1",
            ),
            (
                "<https://example.test/a(b)>",
                "https://example.test/a(b)",
                "https://example.test/a(b",
            ),
            (
                "[doc](https://example.test/a(b)).",
                "https://example.test/a(b)",
                "https://example.test/a(b",
            ),
            (
                "(https://example.test/plain).",
                "https://example.test/plain",
                "https://example.test/plain)",
            ),
            (
                "https://example.test/?value=a)",
                "https://example.test/?value=a)",
                "https://example.test/?value=a",
            ),
            (
                "https://example.test/a(b).",
                "https://example.test/a(b).",
                "https://example.test/a(b)",
            ),
        ] {
            let mut view = VisibleUrls::default();
            view.text(WebUrlSourceKind::UserInput, "", text);
            assert!(
                view.permits(original, &[WebFetchUrlSources::default()]),
                "U/R1 {text}"
            );
            assert!(
                !view.permits(altered, &[WebFetchUrlSources::default()]),
                "U/R2 {text}"
            );
        }
    }
}
