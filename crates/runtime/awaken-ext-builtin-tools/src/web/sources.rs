// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The configured Web extension derives eligibility from the existing transcript
// @formal reason: and consumes that read-only view at its single fetch effect boundary. Frozen
// @formal reason: descriptors own source identity; focused tests own provenance and zero-I/O refusal.
//! URL provenance is a derived Web view of the existing committed transcript.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use awaken_runtime_contract::plugin::{HookReaction, PhaseContext, PhaseHook, PhaseHookPoint};
use awaken_runtime_contract::{ContentBlock, MergePolicy, Message, Role, Scope, Store};
use awaken_tool_contract::tool::{ToolError, current_tool_state};
use awaken_web_policy_contract::{WebFetchUrlSources, WebUrlSourceKind};
use regex::Regex;
use serde::{Deserialize, Serialize};

pub(super) const STATE_KEY: &str = "web_fetch.visible_url_sources";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VisibleUrls {
    urls: BTreeMap<WebUrlSourceKind, BTreeMap<String, BTreeSet<String>>>,
    observed_messages: BTreeSet<String>,
}

fn canonical_url(raw: &str) -> Option<String> {
    let value = url::Url::parse(raw).ok()?;
    matches!(value.scheme(), "http" | "https").then(|| value.to_string())
}

impl VisibleUrls {
    fn load(state: &Store) -> Self {
        state
            .get(
                Scope::Thread,
                &awaken_runtime_contract::StateKey(STATE_KEY.into()),
            )
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
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

    fn observe(&mut self, conversation: &[Message], client_names: &BTreeSet<String>) {
        let mut calls = BTreeMap::new();
        for message in conversation {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, name, .. } if message.role == Role::Assistant => {
                        calls.insert(id.clone(), name.clone());
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } if matches!(message.role, Role::User | Role::Tool) => {
                        if let Some(name) = calls.get(tool_use_id)
                            && !self.observed_messages.contains(&message.id.0)
                        {
                            let kind = if client_names.contains(name) {
                                Some(WebUrlSourceKind::ClientTool)
                            } else if matches!(name.as_str(), "web_search" | "web_fetch") {
                                Some(WebUrlSourceKind::ServerTool)
                            } else {
                                None
                            };
                            if let Some(kind) = kind {
                                self.text(
                                    kind,
                                    name,
                                    &awaken_runtime_contract::extract_text(content),
                                );
                            }
                        }
                    }
                    ContentBlock::Text { text }
                        if message.role == Role::User && !message.id.is_agent_thread_report() =>
                    {
                        self.text(WebUrlSourceKind::UserInput, "", text);
                    }
                    _ => {}
                }
            }
            self.observed_messages.insert(message.id.0.clone());
        }
    }

    fn permits(&self, raw: &str, policies: &[WebFetchUrlSources]) -> bool {
        let Some(url) = canonical_url(raw) else {
            return false;
        };
        // Every inherited policy must have at least one eligible source for
        // this exact URL. Another policy cannot widen a caller's allowed set.
        policies.iter().all(|policy| {
            self.urls.iter().any(|(kind, sources)| {
                sources
                    .iter()
                    .any(|(name, urls)| urls.contains(&url) && policy.permits(*kind, name))
            })
        })
    }
}

pub(super) struct UrlSourceObserver(pub Option<Arc<BTreeSet<String>>>);

#[async_trait]
impl PhaseHook for UrlSourceObserver {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }

    async fn on_phase(
        &self,
        _: &PhaseContext,
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        let mut visible = VisibleUrls::load(state);
        if let Some(client_names) = &self.0 {
            visible.observe(conversation, client_names);
        }
        HookReaction::state(vec![awaken_runtime_contract::StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            STATE_KEY,
            serde_json::to_value(visible).expect("derived URL sources serialize"),
        )])
    }
}

/// The one fetch effect boundary consumes the same Runtime-owned derived view.
/// Direct/mediated invocations without that snapshot cannot invent provenance.
pub(super) fn admit_url(url: &str, policies: &[WebFetchUrlSources]) -> Result<(), ToolError> {
    if policies.is_empty() {
        return Ok(());
    }
    let allowed =
        current_tool_state().is_some_and(|state| VisibleUrls::load(&state).permits(url, policies));
    if allowed {
        Ok(())
    } else {
        Err(ToolError::DefinitiveFailure(
            "url_not_allowed: URL was not shown by an allowed conversation source".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{MessageId, RunId};
    use awaken_tool_contract::tool::with_tool_state_context;
    use serde_json::json;

    #[tokio::test]
    async fn visible_url_source_admission_decision_table() {
        // C1 configured source policy; C2 URL shown in user/tool result; C3 exact
        // allowed tool name; C4 assistant text/arguments, unmatched result or
        // inter-thread report; C5 missing/recovered derived state; C6 inherited
        // narrowing. R1 C2+C3 -> allow; R2 !C2/C4/C5(missing) -> block before
        // the same fetch I/O boundary; R3 recovered State -> same result; R4 every C6 must allow.
        // Tools in one inference cannot authorize URLs not yet shown back to
        // the model: only BeforeInference collects the transcript.
        let url = "https://example.com/visible";
        let policy: WebFetchUrlSources = serde_json::from_value(json!({
            "user_input":"none", "server_tool_results":{"type":"only", "tools":[{"type":"tool_reference", "name":"web_search"}]}, "client_tool_results":"none"
        })).unwrap();
        let mut state = Store::new();
        assert!(admit_url(url, std::slice::from_ref(&policy)).is_err());
        let conversation = vec![
            Message::text(MessageId("model".into()), Role::Assistant, url),
            Message::new(
                MessageId("search".into()),
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
            kind: awaken_runtime_contract::plugin::PhaseKind::BeforeInference {
                run_input: vec![].into(),
            },
        };
        for command in UrlSourceObserver(Some(Arc::new(BTreeSet::new())))
            .on_phase(&ctx, &conversation, &state)
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
        let stored = serde_json::to_value(VisibleUrls::load(&state)).unwrap();
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
            facts.observe(&[message], &BTreeSet::new());
            assert!(!facts.permits(url, &[WebFetchUrlSources::default()]));
        }
        let mut user = VisibleUrls::default();
        user.observe(
            &[Message::text(
                MessageId("u".into()),
                Role::User,
                "See <HTTPS://EXAMPLE.COM/visible>.",
            )],
            &BTreeSet::new(),
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
                    MessageId("call".into()),
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
        view.observe(
            &transcript("web_search"),
            &BTreeSet::from(["web_search".into()]),
        );
        assert!(view.permits(url, std::slice::from_ref(&client)));
        assert!(!view.permits(url, std::slice::from_ref(&server)));
        let mut recovered: VisibleUrls =
            serde_json::from_value(serde_json::to_value(view).unwrap()).unwrap();
        recovered.observe(&transcript("web_search"), &BTreeSet::new());
        assert!(!recovered.permits(url, std::slice::from_ref(&server)));
        assert!(recovered.permits(url, std::slice::from_ref(&client)));
        let mut server_view = VisibleUrls::default();
        server_view.observe(&transcript("web_search"), &BTreeSet::new());
        assert!(server_view.permits(url, std::slice::from_ref(&server)));
        assert!(!server_view.permits(url, std::slice::from_ref(&client)));
        assert!(!server_view.permits(url, &[server.clone(), client]));
        let mut other_tool = VisibleUrls::default();
        other_tool.observe(&transcript("bash"), &BTreeSet::new());
        assert!(!other_tool.permits(url, &[WebFetchUrlSources::default()]));
        let ctx = PhaseContext {
            run_id: RunId("missing-catalogue".into()),
            step: 1,
            kind: awaken_runtime_contract::plugin::PhaseKind::BeforeInference {
                run_input: vec![].into(),
            },
        };
        let mut state = Store::new();
        for command in UrlSourceObserver(None)
            .on_phase(&ctx, &transcript("web_search"), &state)
            .await
            .state
        {
            state.apply(&command);
        }
        assert!(!VisibleUrls::load(&state).permits(url, &[server]));
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
