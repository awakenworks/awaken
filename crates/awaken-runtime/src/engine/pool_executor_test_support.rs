use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceRoutingKey, InferenceStream, LlmExecutor,
    LlmStreamEvent,
};
use awaken_contract::contract::inference::{StopReason, StreamResult};
use awaken_contract::contract::message::Message;
use awaken_contract::registry_spec::{PoolMemberRole, PoolRoutingPolicy, PoolSwitchPolicy};

use super::super::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use super::super::pool_router::{PoolRouter, RouterMember};
use super::{PoolExecutor, PoolMemberExecutor};

pub(super) fn ok_result() -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }
}

pub(super) fn request() -> InferenceRequest {
    InferenceRequest {
        upstream_model: "pool-incoming".into(),
        routing_key: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        system: vec![],
        overrides: None,
        enable_prompt_cache: false,
    }
}

pub(super) fn request_for_thread(thread_id: &str) -> InferenceRequest {
    InferenceRequest {
        routing_key: Some(InferenceRoutingKey::thread(thread_id)),
        ..request()
    }
}

pub(super) fn stream_request_for_thread(thread_id: &str, logical_id: &str) -> InferenceRequest {
    InferenceRequest {
        routing_key: Some(InferenceRoutingKey {
            thread_id: Some(thread_id.to_string()),
            logical_inference_id: Some(logical_id.to_string()),
            ..Default::default()
        }),
        ..request_for_thread(thread_id)
    }
}

pub(super) fn request_for_thread_run(thread_id: &str, run_id: &str) -> InferenceRequest {
    InferenceRequest {
        routing_key: Some(InferenceRoutingKey {
            thread_id: Some(thread_id.to_string()),
            run_id: Some(run_id.to_string()),
            fallback: None,
            logical_inference_id: None,
        }),
        ..request()
    }
}

pub(super) enum Behavior {
    AlwaysOk,
    AlwaysErr(InferenceExecutionError),
    FailTransientThenOk { fails: u32 },
    StreamErr(InferenceExecutionError),
}

pub(super) struct StubExecutor {
    id: String,
    behavior: Behavior,
    calls: AtomicU32,
}

impl StubExecutor {
    fn new(id: &str, behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            behavior,
            calls: AtomicU32::new(0),
        })
    }

    pub(super) fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmExecutor for StubExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            Behavior::AlwaysOk => Ok(ok_result()),
            Behavior::AlwaysErr(err) => Err(err.clone()),
            Behavior::FailTransientThenOk { fails } => {
                if n < *fails {
                    Err(InferenceExecutionError::Provider("transient".into()))
                } else {
                    Ok(ok_result())
                }
            }
            Behavior::StreamErr(_) => Ok(ok_result()),
        }
    }

    fn execute_stream(
        &self,
        _request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::AlwaysErr(err) => Err(err.clone()),
                Behavior::StreamErr(err) => Ok(Box::pin(futures::stream::iter(vec![
                    Ok(LlmStreamEvent::TextDelta("partial".into())),
                    Err(err.clone()),
                ])) as InferenceStream),
                _ => Ok(Box::pin(futures::stream::iter(vec![Ok(LlmStreamEvent::Stop(
                    StopReason::EndTurn,
                ))])) as InferenceStream),
            }
        })
    }

    fn name(&self) -> &str {
        &self.id
    }
}

pub(super) fn router_over(ids: &[String], switch: PoolSwitchPolicy) -> PoolRouter {
    router_over_with_routing(ids, PoolRoutingPolicy::default(), switch)
}

pub(super) fn router_over_with_routing(
    ids: &[String],
    routing: PoolRoutingPolicy,
    switch: PoolSwitchPolicy,
) -> PoolRouter {
    let members = ids
        .iter()
        .map(|id| RouterMember {
            model_id: id.clone(),
            role: PoolMemberRole::Member,
            weight: 1,
        })
        .collect();
    PoolRouter::new(members, routing, switch)
}

pub(super) fn ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("m{i}")).collect()
}

pub(super) fn home_of(home_key: &str, n: usize) -> usize {
    router_over(&ids(n), PoolSwitchPolicy::default()).select_home(home_key, &vec![true; n])
}

pub(super) fn pool_home_fails(
    home_key: &str,
    n: usize,
    home_behavior: Behavior,
    switch: PoolSwitchPolicy,
    breaker: Arc<CircuitBreaker>,
) -> (PoolExecutor, Vec<Arc<StubExecutor>>, usize) {
    let member_ids = ids(n);
    let home = home_of(home_key, n);
    let mut home_behavior = Some(home_behavior);
    let mut stubs = Vec::new();
    let mut member_execs = Vec::new();
    for (i, id) in member_ids.iter().enumerate() {
        let behavior = if i == home {
            home_behavior.take().unwrap()
        } else {
            Behavior::AlwaysOk
        };
        let stub = StubExecutor::new(id, behavior);
        stubs.push(stub.clone());
        member_execs.push(PoolMemberExecutor {
            model_id: id.clone(),
            upstream_model: format!("{id}-upstream"),
            executor: stub as Arc<dyn LlmExecutor>,
        });
    }
    let router = router_over(&member_ids, switch);
    let pool = PoolExecutor::new("pool", home_key, member_execs, router, breaker);
    (pool, stubs, home)
}

pub(super) fn pool_all(
    home_key: &str,
    behaviors: Vec<Behavior>,
    switch: PoolSwitchPolicy,
    breaker: Arc<CircuitBreaker>,
) -> (PoolExecutor, Vec<Arc<StubExecutor>>) {
    pool_all_with_routing(
        home_key,
        behaviors,
        PoolRoutingPolicy::default(),
        switch,
        breaker,
    )
}

pub(super) fn pool_all_with_routing(
    home_key: &str,
    behaviors: Vec<Behavior>,
    routing: PoolRoutingPolicy,
    switch: PoolSwitchPolicy,
    breaker: Arc<CircuitBreaker>,
) -> (PoolExecutor, Vec<Arc<StubExecutor>>) {
    let member_ids = ids(behaviors.len());
    let mut stubs = Vec::new();
    let mut member_execs = Vec::new();
    for (id, behavior) in member_ids.iter().zip(behaviors) {
        let stub = StubExecutor::new(id, behavior);
        stubs.push(stub.clone());
        member_execs.push(PoolMemberExecutor {
            model_id: id.clone(),
            upstream_model: format!("{id}-upstream"),
            executor: stub as Arc<dyn LlmExecutor>,
        });
    }
    let router = router_over_with_routing(&member_ids, routing, switch);
    let pool = PoolExecutor::new("pool", home_key, member_execs, router, breaker);
    (pool, stubs)
}

pub(super) fn breaker() -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default()))
}

pub(super) fn breaker_threshold(n: u32) -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: n,
        ..CircuitBreakerConfig::default()
    }))
}
