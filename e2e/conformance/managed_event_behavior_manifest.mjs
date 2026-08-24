// Causal behavior owners for the SDK-derived Managed Session event catalog.
//
// This is deliberately a rule set, not another event catalog. The complete
// event vocabulary is derived from the installed, exactly pinned SDK by
// `catalog.mjs`; every derived type must match exactly one rule below. Rules
// point at the existing behavior test that owns the event's admission,
// projection, lifecycle, replay, or preview semantics.

export const EVENT_BEHAVIOR_RULES = Object.freeze([
  {
    matches: /^(user\.message|system\.message)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'durable_inbound_projection_uses_only_session_root_provenance',
  },
  {
    matches: /^user\.interrupt$/,
    owner: 'crates/server/awaken-protocol-managed/tests/adapter.rs',
    test: 'interrupt_event_is_acknowledged_without_starting_a_run',
  },
  {
    matches: /^user\.(tool_confirmation|custom_tool_result|tool_result)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'qualified_event_id_routes_every_child_tool_reply_variant',
  },
  {
    matches: /^(user\.define_outcome|span\.outcome_evaluation_(start|ongoing|end))$/,
    owner: 'crates/server/awaken-protocol-managed/tests/adapter.rs',
    test: 'outcome_loop_projects_evaluations',
  },
  {
    matches: /^(agent\.message|agent\.thinking|agent\.thread_message_(received|sent)|session\.thread_created)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'child_message_projection_is_independent_of_refresh_batch_grouping',
  },
  {
    matches: /^agent\.custom_tool_use$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'child_pending_tool_projects_and_replies_through_the_parent_partition',
  },
  {
    matches: /^agent\.(tool_use|tool_result)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'completed_tool_only_child_revisits_withheld_occurrences_before_results',
  },
  {
    matches: /^agent\.mcp_tool_(use|result)$/,
    owner: 'crates/server/awaken-protocol-managed/tests/projection.rs',
    test: 'an_mcp_tool_call_projects_mcp_events',
  },
  {
    matches: /^(agent\.thread_context_compacted|span\.model_request_(start|end))$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'model_request_spans_are_paired_and_keep_required_zero_usage_on_error',
  },
  {
    matches: /^(session\.error|session\.thread_status_terminated)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'ordinary_child_failure_is_error_then_terminated_live_warm_and_cold',
  },
  {
    matches: /^session\.updated$/,
    owner: 'crates/server/awaken-protocol-managed/tests/projection.rs',
    test: 'updating_a_session_commits_a_session_updated_event',
  },
  {
    matches: /^session\.deleted$/,
    owner: 'crates/server/awaken-protocol-managed/tests/mcp_sessions.rs',
    test: 'delete_session_commits_the_deleted_fact_with_the_owner',
  },
  {
    matches: /^session\.(status_(running|idle)|thread_status_(running|idle)|usage)$/,
    owner: 'crates/server/awaken-protocol-managed/src/state/events/tests.rs',
    test: 'recovered_terminal_brackets_output_in_warm_and_cold_projections',
  },
  {
    matches: /^session\.(status_rescheduled|thread_status_rescheduled)$/,
    owner: 'crates/server/awaken-protocol-managed/tests/projection.rs',
    test: 'a_rescheduled_run_projects_the_complete_thread_status_sequence',
  },
  {
    matches: /^session\.status_terminated$/,
    owner: 'crates/server/awaken-protocol-managed/tests/projection.rs',
    test: 'archiving_commits_a_terminal_event_and_fences_writes',
  },
  {
    matches: /^(event_start|event_delta)$/,
    owner: 'crates/server/awaken-protocol-managed/tests/streaming.rs',
    test: 'root_stream_immediately_projects_thread_live_observations_only',
  },
]);

export function eventBehaviorOwners(types, rules = EVENT_BEHAVIOR_RULES) {
  return types.map((type) => {
    const matches = rules.filter((rule) => rule.matches.test(type));
    if (matches.length !== 1) {
      throw new Error(`${type}: expected one behavior owner, found ${matches.length}`);
    }
    return Object.freeze({ type, owner: matches[0].owner, test: matches[0].test });
  });
}
