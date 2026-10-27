// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The one routed fetch effect consumes frozen source/domain policy before
// @formal reason: credentials or provider I/O; fallback is allowed only before dispatch.
//! The configured WebFetch effect boundary and its policy kernel.

use super::{
    EffectiveWebFetchConfiguration, WEB_FETCH_TOOL_ID, WebFetchArgs, WebFetchProvider,
    WebFetchProviderDescriptor, WebFetchRequest, WebProviderTarget, WebSearchCredentialResolver,
    resolve_credential, sources,
};
use async_trait::async_trait;
use awaken_tool_contract::tool::{Tool, ToolError};
use std::sync::Arc;

pub(super) fn admission_error(error: awaken_web_policy_contract::WebAdmissionFailure) -> ToolError {
    match error {
        awaken_web_policy_contract::WebAdmissionFailure::InvalidConfiguration(_)
        | awaken_web_policy_contract::WebAdmissionFailure::InvalidUrl(_) => {
            ToolError::InvalidArguments(error.to_string())
        }
        _ => ToolError::DefinitiveFailure(error.to_string()),
    }
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

/// Process-local proof of ordering, created only after pure Web admission.
/// It is neither serializable nor a cross-process execution authorization.
struct AdmittedFetch {
    raw_url: String,
}

impl AdmittedFetch {
    fn admit(
        args: WebFetchArgs,
        configuration: Option<&EffectiveWebFetchConfiguration>,
        evidence: Option<&sources::VisibleUrls>,
    ) -> Result<Self, ToolError> {
        let facts = evidence.map(sources::VisibleUrls::evidence);
        let domains = configuration.map_or(&[][..], |value| value.domains.as_slice());
        let source_policies = configuration.map_or(&[][..], |value| value.url_sources.as_slice());
        awaken_web_policy_contract::admit_fetch_url(
            &args.url,
            domains,
            source_policies,
            facts.as_deref(),
        )
        .map_err(admission_error)?;
        Ok(Self { raw_url: args.url })
    }
}

#[async_trait]
impl Tool for RoutedWebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;
    const ID: &'static str = WEB_FETCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Fetch a URL through the configured platform provider";

    async fn call(&self, args: WebFetchArgs) -> Result<String, ToolError> {
        let evidence = awaken_tool_contract::tool::current_tool_state()
            .and_then(|state| sources::VisibleUrls::load(&state));
        let admitted = AdmittedFetch::admit(
            args,
            self.execution_configuration.as_ref(),
            evidence.as_ref(),
        )?;
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
                url: admitted.raw_url.clone(),
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
                    if let Some(tokens) = self
                        .execution_configuration
                        .as_ref()
                        .and_then(|configuration| configuration.max_content_tokens)
                    {
                        super::content_budget::EstimatedTokenBudget::new(tokens)
                            .truncate(&mut body);
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
    async fn estimated_content_budget_applies_after_routing_with_nearest_precedence_and_pdf_parity()
    {
        // C1 own/caller/root/absent budget; C2 zero/finite/huge; C3 normal/PDF
        // URL; C4 primary success/pre-dispatch fallback/post-dispatch failure.
        // Effects: same multilingual prefix at the single effect boundary;
        // nearest scalar wins (not minimum), no setting means no projection,
        // and content settings cannot enable fallback after dispatch.
        use awaken_agent_contract::toolset::InheritedToolsets;
        use awaken_agent_contract::{
            ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        struct Provider {
            id: &'static str,
            outcome: u8,
            calls: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl WebFetchProvider for Provider {
            fn descriptor(&self) -> WebFetchProviderDescriptor {
                WebFetchProviderDescriptor {
                    id: self.id.into(),
                    label: self.id.into(),
                    credential: WebSearchCredentialRequirement::None,
                    options_schema: json!({"type":"object"}),
                }
            }
            async fn fetch(
                &self,
                request: WebFetchRequest,
                _: Option<&Credential>,
                _: &[WebDomainFilter],
            ) -> Result<String, ToolError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", self.id, request.url));
                match self.outcome {
                    1 => Err(ToolError::UnavailableBeforeDispatch("not sent".into())),
                    2 => Err(ToolError::DefinitiveFailure("already sent".into())),
                    _ => Ok("ab中🙂cdef".into()),
                }
            }
        }
        let toolsets = |limit: Option<u64>| {
            limit
                .map(|limit| ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy::default(),
                    overrides: vec![ToolPolicyOverride::with_optional_configuration(
                        "web_fetch",
                        ToolExecutionPolicy::default(),
                        Some(json!({"type":"web_fetch","max_content_tokens":limit})),
                    )],
                })
                .into_iter()
                .collect()
        };
        for (own, caller, root, expected) in [
            (None, None, None, "ab中🙂cdef"),
            (Some(0), Some(2), Some(3), ""),
            (Some(3), Some(1), Some(0), "ab中🙂cd"),
            (None, Some(2), Some(1), "ab中"),
            (None, None, Some(1), "ab"),
            (None, Some(0), Some(3), ""),
            (None, None, Some(0), ""),
            (None, Some(u64::MAX), Some(0), "ab中🙂cdef"),
            (Some(1), None, Some(u64::MAX), "ab"),
            (Some(u64::MAX), Some(0), None, "ab中🙂cdef"),
        ] {
            let inherited = InheritedToolsets::with_caller(
                toolsets(caller),
                Some(&InheritedToolsets::with_caller(toolsets(root), None)),
            );
            for suffix in ["/page", "/guide.PDF?download=true"] {
                for outcome in 0..=2 {
                    let calls = Arc::new(Mutex::new(Vec::new()));
                    let mut registry = WebSearchProviderRegistry::default();
                    for (id, result) in [("primary-budget", outcome), ("fallback-budget", 0)] {
                        registry
                            .register_fetch(Arc::new(Provider {
                                id,
                                outcome: result,
                                calls: calls.clone(),
                            }))
                            .unwrap();
                    }
                    let plugin = WebFetchPlugin::new(registry, None)
                        .with_inherited_execution_configuration(
                            own.map(|limit| WebFetchExecutionConfiguration {
                                max_content_tokens: Some(limit),
                                ..Default::default()
                            }),
                            &inherited,
                        )
                        .unwrap();
                    let (_, tool) = plugin.configured_tool(Some(&json!({"provider_id":"primary-budget","options":{},"fallbacks":[{"provider_id":"fallback-budget","options":{}}]}))).unwrap();
                    let url = format!("https://example.com{suffix}");
                    let result = tool
                        .invoke(ToolCall {
                            call_id: "budget-probe".into(),
                            tool_id: "web_fetch".into(),
                            arguments: json!({"url":url}),
                        })
                        .await;
                    let seen = calls.lock().unwrap();
                    assert_eq!(seen[0], format!("primary-budget:{url}"));
                    if outcome == 2 {
                        assert!(result.unwrap_err().to_string().contains("already sent"));
                        assert_eq!(seen.len(), 1, "post-dispatch failure cannot fallback");
                    } else {
                        assert_eq!(
                            result.unwrap().text(),
                            expected,
                            "own={own:?} caller={caller:?} root={root:?} suffix={suffix} outcome={outcome}"
                        );
                        assert_eq!(seen.len(), if outcome == 1 { 2 } else { 1 });
                        if outcome == 1 {
                            assert_eq!(seen[1], format!("fallback-budget:{url}"));
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn large_estimated_budget_cannot_relax_direct_http_response_byte_fence() {
        // A real native loopback response exceeds the independent 1 MiB read
        // fence. An effectively unlimited content preference cannot widen it.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        const EXPECTED_RESPONSE_BYTES: usize = 1_048_576;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let body = vec![b'x'; EXPECTED_RESPONSE_BYTES + 64];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            // The bounded reader can intentionally close before the last bytes.
            let _ = stream.write_all(&body);
        });
        let (_, tool) = WebFetchPlugin::new(WebSearchProviderRegistry::builtins(), None)
            .with_execution_configuration(Some(WebFetchExecutionConfiguration {
                max_content_tokens: Some(u64::MAX),
                ..Default::default()
            }))
            .configured_tool(None)
            .unwrap();
        let output = tool
            .invoke(ToolCall {
                call_id: "byte-fence".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({"url":format!("http://{address}/large.pdf")}),
            })
            .await
            .unwrap();
        worker.join().unwrap();
        assert_eq!(output.text().len(), EXPECTED_RESPONSE_BYTES);
        assert!(output.text().bytes().all(|byte| byte == b'x'));
    }

    fn observed_phase(
        run_id: &str,
        _: &[awaken_agent_contract::agent::message::Message],
    ) -> awaken_runtime_contract::plugin::PhaseContext {
        awaken_runtime_contract::plugin::PhaseContext {
            run_id: awaken_agent_contract::agent::run::Id(run_id.into()),
            step: 0,
            kind: awaken_runtime_contract::plugin::PhaseKind::AfterInference,
        }
    }

    fn observed_input(
        conversation: &[awaken_agent_contract::agent::message::Message],
    ) -> Vec<awaken_runtime_contract::llm::ChatMessage> {
        conversation
            .iter()
            .map(|message| awaken_runtime_contract::llm::ChatMessage {
                role: message.role,
                content: message.content.clone(),
            })
            .collect()
    }

    #[tokio::test]
    async fn direct_http_provider_refuses_redirect_before_the_next_connection() {
        // A real loopback 302 points from an allowed host to a blocked host.
        // The second listener counts actual requests; a wake-only connection
        // lets the owned fixture exit without sleeps or a leftover process.
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let target_worker = std::thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            if stream.read(&mut request).unwrap() != 0 {
                observed.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nescaped",
                    )
                    .unwrap();
            }
        });
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let first_address = redirect.local_addr().unwrap();
        let first_hits = Arc::new(AtomicUsize::new(0));
        let first_observed = first_hits.clone();
        let first_worker = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let mut request = [0; 4096];
            if stream.read(&mut request).unwrap() != 0 {
                first_observed.fetch_add(1, Ordering::SeqCst);
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target_address.port()).unwrap();
            }
        });
        let (_, tool) = WebFetchPlugin::new(WebSearchProviderRegistry::builtins(), None)
            .with_execution_configuration(Some(WebFetchExecutionConfiguration {
                domains: Some(WebDomainFilter::Allow(vec!["127.0.0.1".into()])),
                ..Default::default()
            }))
            .configured_tool(None)
            .unwrap();
        let result = tool
            .invoke(ToolCall {
                call_id: "redirect-probe".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({"url":format!("http://{first_address}/allowed")}),
            })
            .await;
        if let Ok(wake) = TcpStream::connect(target_address) {
            wake.shutdown(Shutdown::Both).unwrap();
        }
        if let Ok(wake) = TcpStream::connect(first_address) {
            wake.shutdown(Shutdown::Both).unwrap();
        }
        first_worker.join().unwrap();
        target_worker.join().unwrap();
        assert!(
            matches!(result, Err(ToolError::DefinitiveFailure(_))),
            "{result:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "redirect target receives no HTTP request"
        );
        assert_eq!(
            first_hits.load(Ordering::SeqCst),
            1,
            "the allowed first hop actually executes"
        );
    }

    #[tokio::test]
    async fn refused_request_never_resolves_credentials_or_dispatches_paid_provider() {
        // R1 missing evidence -> zero credential/provider calls; R2 evidence
        // for a URL outside the domain policy -> the same zero-effect result.
        // R3 admitted URL -> exactly one credential attempt, whose failure
        // prevents dispatch and fallback. Both public executable entry points
        // share the private request construction above.
        use awaken_agent_contract::agent::{
            message::{Id as MessageId, Message, Role},
            state::Store,
        };
        use awaken_tool_contract::tool::with_tool_state_context;
        use awaken_web_policy_contract::WebFetchUrlSources;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Credentials(AtomicUsize);
        #[async_trait]
        impl WebSearchCredentialResolver for Credentials {
            async fn resolve(&self, _: &CredentialAccess, _: &str) -> Result<Credential, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("credential probe reached".into())
            }
        }
        struct Paid(Arc<AtomicUsize>);
        #[async_trait]
        impl WebFetchProvider for Paid {
            fn descriptor(&self) -> WebFetchProviderDescriptor {
                WebFetchProviderDescriptor {
                    id: "paid-probe".into(),
                    label: "Paid probe".into(),
                    credential: WebSearchCredentialRequirement::Exact(
                        CredentialUsage::HttpHeader {
                            name: "Authorization".into(),
                            scheme: None,
                        },
                    ),
                    options_schema: json!({"type":"object"}),
                }
            }
            fn enforces_domain_filter(&self) -> bool {
                true
            }
            async fn fetch(
                &self,
                _: WebFetchRequest,
                _: Option<&Credential>,
                _: &[WebDomainFilter],
            ) -> Result<String, ToolError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok("body".into())
            }
        }
        // C4 absent/zero budget: neither can bypass URL or credential admission.
        for content_budget in [None, Some(0)] {
            let credentials = Arc::new(Credentials(AtomicUsize::new(0)));
            let calls = Arc::new(AtomicUsize::new(0));
            let mut registry = WebSearchProviderRegistry::default();
            registry
                .register_fetch(Arc::new(Paid(calls.clone())))
                .unwrap();
            let holder = awaken_agent_contract::credential::PlaintextHolder::new(
                PlaintextBoundary::Worker,
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            );
            let access = CredentialAccess::new(
                CredentialRef {
                    id: "cred:paid-probe".into(),
                    revision: 1,
                },
                awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "Authorization".into(),
                    scheme: None,
                },
                awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                    holder,
                    ModelExposurePolicy::Forbidden,
                ),
            )
            .with_target(CredentialTarget::new(
                CredentialPurpose::WebProviderAuthorization,
                "paid-probe",
            ));
            let config =
                json!({"provider_id":"paid-probe", "credential_access":access, "options":{}});
            let plugin = WebFetchPlugin::new(registry.clone(), Some(credentials.clone()))
                .with_tool_source_catalogue(&[])
                .with_execution_configuration(Some(WebFetchExecutionConfiguration {
                    url_sources: Some(WebFetchUrlSources::default()),
                    domains: Some(WebDomainFilter::Allow(vec!["example.com".into()])),
                    max_content_tokens: content_budget,
                }));
            let (_, direct) = plugin.configured_tool(Some(&config)).unwrap();
            let contributions = plugin.resolve_configured(Some(&config)).unwrap();
            let call = |url: &str| ToolCall {
                call_id: "probe".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({"url":url}),
            };
            for executable in [
                direct.as_ref(),
                contributions.dynamic_tools[0].executable().as_ref(),
            ] {
                assert!(
                    executable
                        .invoke(call("https://example.com/shown"))
                        .await
                        .is_err()
                );
            }
            assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let mut state = Store::new();
            let conversation = vec![Message::text(
                MessageId("u".into()),
                Role::User,
                "https://outside.test/shown https://example.com/shown",
            )];
            let ctx = observed_phase("probe", &conversation);
            for hook in &contributions.phase_hooks {
                for command in hook
                    .on_model_input(&ctx, &observed_input(&conversation), &conversation, &state)
                    .await
                    .state
                {
                    state.apply(&command);
                }
            }
            with_tool_state_context(state.clone(), async {
                assert!(
                    direct
                        .invoke(call("https://outside.test/shown"))
                        .await
                        .is_err()
                );
            })
            .await;
            assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
            with_tool_state_context(state, async {
                assert!(
                    direct
                        .invoke(call("https://example.com/shown"))
                        .await
                        .is_err()
                );
            })
            .await;
            assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let (_, unconfigured) = WebFetchPlugin::new(registry, Some(credentials.clone()))
                .configured_tool(Some(&config))
                .unwrap();
            for bad in [
                "not a URL",
                "file:///private",
                "ftp://example.com/file",
                "mailto:user@example.com",
                "http://",
            ] {
                assert!(
                    matches!(
                        unconfigured.invoke(call(bad)).await,
                        Err(ToolError::InvalidArguments(_))
                    ),
                    "{bad}"
                );
            }
            assert_eq!(
                credentials.0.load(Ordering::SeqCst),
                1,
                "even unrestricted fetch validates before credentials"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(
                unconfigured
                    .invoke(call("https://example.com/valid"))
                    .await
                    .is_err()
            );
            assert_eq!(
                credentials.0.load(Ordering::SeqCst),
                2,
                "valid unrestricted URL reaches the credential positive control"
            );
        }
    }

    #[tokio::test]
    async fn url_sources_guard_the_single_fetch_effect_boundary() {
        // Causes: C1 explicit URL-source policy; C2 missing/current/restored
        // Runtime-derived snapshot; C3 shown/unseen URL; C4 configured/direct
        // executable entry; C5 inherited restriction. Effects: R1 C1+missing
        // or unseen -> url_not_allowed with zero provider calls; R2 current
        // eligible URL -> exactly one effect; R3 restored view -> same rule;
        // R4 every C5 is applied. No second gate can be bypassed by direct call.
        use awaken_agent_contract::agent::{
            message::{Id as MessageId, Message, Role},
            state::Store,
        };
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
        // R5's dependency is a registered provider-server fetch, witnessed by
        // the unconfigured positive control. An unknown provider error cannot
        // substitute for refusal of an unenforceable source restriction.
        let server_config = json!({"provider_id":OPENROUTER_PROVIDER_ID,"options":{}});
        let (server_descriptor, _) = WebFetchPlugin::new(registry.clone(), None)
            .configured_tool(Some(&server_config))
            .unwrap();
        assert!(
            server_descriptor.provider_server_tool.is_some(),
            "R5 registered control"
        );
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
        let refusal = plugin
            .configured_tool(Some(&server_config))
            .err()
            .expect("R5 source policy refuses server realization");
        assert!(
            refusal
                .to_string()
                .contains("cannot enforce the Agent WebFetch execution policy"),
            "R5 policy cause: {refusal}"
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
        let conversation = vec![Message::text(MessageId("user".into()), Role::User, url)];
        let ctx = observed_phase("source-probe", &conversation);
        for hook in &contributions.phase_hooks {
            for command in hook
                .on_model_input(&ctx, &observed_input(&conversation), &conversation, &state)
                .await
                .state
            {
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
        // This consumes the observed model input at the same boundary,
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
            let ctx = observed_phase("source-probe", &conversation);
            for hook in &contributions.phase_hooks {
                for command in hook
                    .on_model_input(&ctx, &observed_input(&conversation), &conversation, &state)
                    .await
                    .state
                {
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
