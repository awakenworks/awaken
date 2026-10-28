use awaken_session_budget_contract::{SessionModelUsage, SessionUsage};

#[must_use]
pub fn session_usage_from_thread_usage(
    attributed: awaken_agent_contract::thread::usage::ThreadUsage,
) -> SessionUsage {
    let total = attributed.total();
    SessionUsage {
        input_tokens: total.prompt_tokens,
        output_tokens: total.completion_tokens,
        cache_read_tokens: total.cache_read_tokens,
        cache_creation_tokens: total.cache_creation_tokens,
        by_model: attributed
            .by_model
            .into_iter()
            .map(|(model, usage)| {
                (
                    model,
                    SessionModelUsage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                )
            })
            .collect(),
        active_seconds: 0,
        web_fetch_requests: 0,
        web_search_requests: 0,
    }
}
