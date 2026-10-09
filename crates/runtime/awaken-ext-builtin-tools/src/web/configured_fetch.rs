// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The one routed fetch effect consumes frozen source/domain policy before
// @formal reason: credentials or provider I/O; fallback is allowed only before dispatch.
//! The configured WebFetch effect boundary and its policy kernel.

use super::{
    EffectiveWebFetchConfiguration, WEB_FETCH_TOOL_ID, WebFetchArgs, WebFetchProvider,
    WebFetchProviderDescriptor, WebFetchRequest, WebProviderTarget, WebSearchCredentialResolver,
    resolve_credential, sources, url_matches_filters,
};
use async_trait::async_trait;
use awaken_tool_contract::tool::{Tool, ToolError};
use std::sync::Arc;

pub(super) fn configured_web_fetch_url(
    raw_url: &str,
    configuration: &EffectiveWebFetchConfiguration,
) -> Result<url::Url, ToolError> {
    let url = url::Url::parse(raw_url)
        .map_err(|error| ToolError::InvalidArguments(format!("url: {error}")))?;
    if !url_matches_filters(&url, &configuration.domains) {
        return Err(ToolError::DefinitiveFailure(
            "url_not_allowed: web_fetch URL is outside the combined domain policy".into(),
        ));
    }
    Ok(url)
}

pub(super) fn web_fetch_text_limit(
    url: &url::Url,
    configuration: &EffectiveWebFetchConfiguration,
) -> Option<usize> {
    configuration
        .max_content_tokens
        // Binary content is not part of the configured text-context cap.
        // Preserve the legacy PDF URL exception for providers returning lossy
        // text while every provider keeps its independent response-size fence.
        .filter(|_| !url.path().to_ascii_lowercase().ends_with(".pdf"))
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
}

pub(super) fn truncate_text(text: &mut String, max_bytes: usize) {
    let mut boundary = max_bytes.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}

pub(super) struct ConfiguredFetchTarget {
    pub(super) provider: Arc<dyn WebFetchProvider>,
    pub(super) descriptor: WebFetchProviderDescriptor,
    pub(super) config: WebProviderTarget,
}

pub(super) struct RoutedWebFetchTool {
    pub(super) targets: Vec<ConfiguredFetchTarget>,
    pub(super) credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    pub(super) execution_configuration: Option<EffectiveWebFetchConfiguration>,
}

#[async_trait]
impl Tool for RoutedWebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;
    const ID: &'static str = WEB_FETCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Fetch a URL through the configured platform provider";

    async fn call(&self, args: WebFetchArgs) -> Result<String, ToolError> {
        if let Some(configuration) = &self.execution_configuration {
            sources::admit_url(&args.url, &configuration.url_sources)?;
        }
        let configured_url = self
            .execution_configuration
            .as_ref()
            .map(|configuration| configured_web_fetch_url(&args.url, configuration))
            .transpose()?;
        let mut last_unavailable = None;
        for target in &self.targets {
            let credential = resolve_credential(
                &target.descriptor.credential,
                target.config.credential_access.as_ref(),
                self.credentials.as_ref(),
                &target.descriptor.id,
                "web-fetch",
            )
            .await?;
            let request = WebFetchRequest {
                url: args.url.clone(),
                options: target.config.options.clone(),
            };
            let domain_filters = self
                .execution_configuration
                .as_ref()
                .map_or(&[][..], |configuration| configuration.domains.as_slice());
            let result = target
                .provider
                .fetch(request, credential.as_ref(), domain_filters)
                .await;
            match result {
                Ok(mut body) => {
                    if let (Some(url), Some(configuration)) = (
                        configured_url.as_ref(),
                        self.execution_configuration.as_ref(),
                    ) && let Some(max_bytes) = web_fetch_text_limit(url, configuration)
                    {
                        truncate_text(&mut body, max_bytes);
                    }
                    return Ok(body);
                }
                Err(error @ ToolError::UnavailableBeforeDispatch(_)) => {
                    last_unavailable = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_unavailable.unwrap_or_else(|| {
            ToolError::DefinitiveFailure("web-fetch route plan has no targets".into())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::FakeFetchProvider;
    use super::super::*;
    use awaken_tool_contract::tool::ToolCall;
    use std::sync::Mutex;

    #[tokio::test]
    async fn url_sources_guard_the_single_fetch_effect_boundary() {
        // Causes: C1 explicit URL-source policy; C2 missing/current/restored
        // Runtime-derived snapshot; C3 shown/unseen URL; C4 configured/direct
        // executable entry; C5 inherited restriction. Effects: R1 C1+missing
        // or unseen -> url_not_allowed with zero provider calls; R2 current
        // eligible URL -> exactly one effect; R3 restored view -> same rule;
        // R4 every C5 is applied. No second gate can be bypassed by direct call.
        use awaken_runtime_contract::plugin::{PhaseContext, PhaseKind};
        use awaken_runtime_contract::{Message, MessageId, Role, RunId, Store};
        use awaken_tool_contract::tool::with_tool_state_context;
        use awaken_web_policy_contract::WebFetchUrlSources;
        let provider = Arc::new(FakeFetchProvider {
            seen_requests: Mutex::new(Vec::new()),
            seen_filters: Mutex::new(Vec::new()),
        });
        let mut registry = WebSearchProviderRegistry::builtins();
        registry.register_fetch(provider.clone()).unwrap();
        let sources: WebFetchUrlSources = serde_json::from_value(
            json!({"client_tool_results":"none","server_tool_results":"none"}),
        )
        .unwrap();
        let plugin = WebFetchPlugin::new(registry, None)
            .with_tool_source_catalogue(&[])
            .with_execution_configuration(Some(WebFetchExecutionConfiguration {
                url_sources: Some(sources),
                domains: None,
                max_content_tokens: None,
            }));
        let config = json!({"provider_id":"fetch-probe","options":{}});
        // R5 a provider-server realization cannot consume this transcript
        // view: reject its configuration rather than dropping provenance.
        assert!(
            plugin
                .configured_tool(Some(
                    &json!({"provider_id":ANTHROPIC_PROVIDER_ID,"options":{}})
                ))
                .is_err()
        );
        let contributions = plugin.resolve_configured(Some(&config)).unwrap();
        assert!(
            contributions.tool_gates.is_empty(),
            "single I/O-boundary owner"
        );
        let (_, direct) = plugin.configured_tool(Some(&config)).unwrap();
        let call = |url: &str| ToolCall {
            call_id: "url-source-probe".into(),
            tool_id: "web_fetch".into(),
            arguments: json!({"url":url}),
        };
        let url = "https://example.com/from-user";
        assert!(
            direct
                .invoke(call(url))
                .await
                .unwrap_err()
                .to_string()
                .contains("url_not_allowed")
        );
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            0,
            "R1 zero direct effect"
        );
        let mut state = Store::new();
        let ctx = PhaseContext {
            run_id: RunId("source-probe".into()),
            step: 0,
            kind: PhaseKind::BeforeInference {
                run_input: vec![].into(),
            },
        };
        let conversation = vec![Message::text(MessageId("user".into()), Role::User, url)];
        for hook in &contributions.phase_hooks {
            for command in hook.on_phase(&ctx, &conversation, &state).await.state {
                state.apply(&command);
            }
        }
        with_tool_state_context(state.clone(), async {
            assert!(
                direct
                    .invoke(call("https://example.com/unseen"))
                    .await
                    .is_err()
            );
            assert_eq!(
                provider.seen_requests.lock().unwrap().len(),
                0,
                "R1 unseen zero effect"
            );
            direct.invoke(call(url)).await.unwrap();
        })
        .await;
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            1,
            "R2 one direct effect"
        );
        with_tool_state_context(state, async {
            contributions.dynamic_tools[0]
                .executable()
                .invoke(call(url))
                .await
                .unwrap();
        })
        .await;
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            2,
            "R3 same configured effect boundary"
        );
        // U1/U2/U3 literal IPv6, balanced path parentheses, and query ')'
        // must remain whole. R6 original shown URI -> one provider effect;
        // its former trimmed variant was NOT shown -> zero additional I/O.
        // This consumes the actual BeforeInference view at the same boundary,
        // complementing the scanner's pure decision table rather than copying it.
        for (text, original, altered) in [
            ("http://[::1]", "http://[::1]", "http://[::1"),
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
                "https://example.test/?value=a)",
                "https://example.test/?value=a)",
                "https://example.test/?value=a",
            ),
        ] {
            let mut state = Store::new();
            let conversation = vec![Message::text(MessageId("uri".into()), Role::User, text)];
            for hook in &contributions.phase_hooks {
                for command in hook.on_phase(&ctx, &conversation, &state).await.state {
                    state.apply(&command);
                }
            }
            let before = provider.seen_requests.lock().unwrap().len();
            with_tool_state_context(state, async {
                assert!(
                    direct.invoke(call(altered)).await.is_err(),
                    "R6 altered {text}"
                );
                assert_eq!(
                    provider.seen_requests.lock().unwrap().len(),
                    before,
                    "R6 zero I/O"
                );
                direct.invoke(call(original)).await.unwrap();
            })
            .await;
            assert_eq!(
                provider.seen_requests.lock().unwrap().len(),
                before + 1,
                "R6 original {text}"
            );
        }
    }
}
