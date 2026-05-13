import { describe, expect, test } from "vitest";
import {
  type AgentSpec,
  type ContextWindowPolicy,
  type RemoteEndpoint,
  DEFAULT_CONTEXT_POLICY,
} from "./types";

describe("AgentSpec round-trip", () => {
  test("preserves context_policy / active_hook_filter / endpoint / registry through JSON", () => {
    const policy: ContextWindowPolicy = {
      ...DEFAULT_CONTEXT_POLICY,
      autocompact_threshold: 150_000,
    };
    const endpoint: RemoteEndpoint = {
      backend: "a2a",
      base_url: "https://remote.example.com",
      auth: { type: "bearer", token: "redacted-but-shaped" },
      target: "remote-agent",
      timeout_ms: 60_000,
      options: { poll_interval_ms: 250 },
    };
    const spec: AgentSpec = {
      id: "alpha",
      model_id: "research-default",
      system_prompt: "You are useful.",
      max_rounds: 8,
      context_policy: policy,
      plugin_ids: ["mcp", "permission"],
      active_hook_filter: ["permission"],
      sections: {},
      delegates: [],
      endpoint,
      registry: "cloud",
    };

    const roundTripped = JSON.parse(JSON.stringify(spec)) as AgentSpec;
    expect(roundTripped).toEqual(spec);
    // Sanity: the fields that previously silently dropped on PUT are present.
    expect(roundTripped.context_policy).toEqual(policy);
    expect(roundTripped.active_hook_filter).toEqual(["permission"]);
    expect(roundTripped.endpoint).toEqual(endpoint);
    expect(roundTripped.registry).toBe("cloud");
  });

  test("null context_policy serializes as null (policy disabled)", () => {
    const spec: AgentSpec = {
      id: "a",
      model_id: "m",
      system_prompt: "",
      context_policy: null,
    };
    const json = JSON.stringify(spec);
    expect(JSON.parse(json).context_policy).toBeNull();
  });

  test("empty active_hook_filter round-trips as empty array (no filtering)", () => {
    const spec: AgentSpec = {
      id: "a",
      model_id: "m",
      system_prompt: "",
      active_hook_filter: [],
    };
    expect(JSON.parse(JSON.stringify(spec)).active_hook_filter).toEqual([]);
  });

  // Regression coverage for R3 #8. The Rust `RemoteEndpoint` deserializer
  // accepts legacy aliases (`bearer_token`, `agent_id`, `poll_interval_ms`)
  // that pre-date the new `auth` / `target` / `options` shape. The TS type
  // uses an index signature so unknown keys travel through editor state
  // verbatim; this test pins that behaviour so a future tightening of the
  // TS type doesn't accidentally drop legacy data on the floor.
  test("legacy RemoteEndpoint aliases round-trip without normalization", () => {
    // Shape emitted by older configs before the `auth: {type, ...}` block
    // and `target` / `options` keys were introduced.
    const legacyEndpointJson = JSON.stringify({
      backend: "a2a",
      base_url: "https://legacy.example.com",
      bearer_token: "legacy-bearer",
      agent_id: "legacy-target",
      poll_interval_ms: 1000,
    });
    const legacy = JSON.parse(legacyEndpointJson) as RemoteEndpoint;

    // Even though the TS type doesn't declare `bearer_token` / `agent_id` /
    // `poll_interval_ms`, they survive the editor state cycle because
    // `RemoteEndpoint` permits unknown keys via index signature. The
    // editor never normalises locked-field shapes (endpoint is read-only),
    // so a save that PATCHes other fields can't drop these on the wire.
    const cast = legacy as unknown as Record<string, unknown>;
    expect(cast.bearer_token).toBe("legacy-bearer");
    expect(cast.agent_id).toBe("legacy-target");
    expect(cast.poll_interval_ms).toBe(1000);

    // Round-tripping the legacy endpoint through JSON preserves every key.
    const roundTripped = JSON.parse(JSON.stringify(legacy)) as Record<string, unknown>;
    expect(roundTripped).toEqual({
      backend: "a2a",
      base_url: "https://legacy.example.com",
      bearer_token: "legacy-bearer",
      agent_id: "legacy-target",
      poll_interval_ms: 1000,
    });

    // When the legacy endpoint lives inside an AgentSpec, the full spec
    // round-trip must not drop or rename any of its keys either — that's
    // the actual editor-state path (queryClient hydrate → setSpec → Save).
    const spec: AgentSpec = {
      id: "legacy-agent",
      model_id: "m",
      system_prompt: "p",
      endpoint: legacy,
    };
    const specRoundTrip = JSON.parse(JSON.stringify(spec)) as AgentSpec;
    expect(specRoundTrip.endpoint).toEqual(legacy);
  });
});

describe("DEFAULT_CONTEXT_POLICY", () => {
  test("mirrors Rust ContextWindowPolicy::default()", () => {
    expect(DEFAULT_CONTEXT_POLICY).toEqual({
      max_context_tokens: 200_000,
      max_output_tokens: 16_384,
      min_recent_messages: 10,
      enable_prompt_cache: true,
      autocompact_threshold: null,
      compaction_mode: "keep_recent_raw_suffix",
      compaction_raw_suffix_messages: 2,
    });
  });
});
