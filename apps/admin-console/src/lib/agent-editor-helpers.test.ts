import { describe, expect, it } from "vitest";

import type { AgentSpec, RemoteEndpoint } from "./api/types";
import {
  ALLOWED_AGENT_FIELDS,
  PATCHABLE_AGENT_FIELDS,
  canonicalStringify,
  cloneAgentSpecForEditor,
  deepEqualCanonical,
  diffPatchableAgentFields,
  lockedFieldChange,
  mergeLockedFields,
  partitionActiveHookFilter,
  redactAgentSpecForDisplay,
  redactEndpointForDisplay,
  togglePluginState,
  unknownAgentSpecFields,
} from "./agent-editor-helpers";

function baseSpec(overrides: Partial<AgentSpec> = {}): AgentSpec {
  return {
    id: "alpha",
    model_id: "research-default",
    system_prompt: "You are a test agent.",
    max_rounds: 8,
    plugin_ids: [],
    active_hook_filter: [],
    sections: {},
    delegates: [],
    ...overrides,
  };
}

const REMOTE_ENDPOINT: RemoteEndpoint = {
  backend: "a2a",
  base_url: "https://remote.example.com",
  target: "remote-agent",
  timeout_ms: 60_000,
};

describe("canonicalStringify / deepEqualCanonical", () => {
  it("sorts object keys deterministically", () => {
    expect(canonicalStringify({ b: 2, a: 1 })).toBe(canonicalStringify({ a: 1, b: 2 }));
  });

  it("recursively sorts nested keys", () => {
    expect(
      canonicalStringify({ outer: { y: 2, x: 1 }, list: [{ b: 2, a: 1 }] }),
    ).toBe(canonicalStringify({ list: [{ a: 1, b: 2 }], outer: { x: 1, y: 2 } }));
  });

  it("preserves array order (semantically significant)", () => {
    expect(deepEqualCanonical(["a", "b"], ["b", "a"])).toBe(false);
  });

  it("treats undefined and absent equivalently", () => {
    expect(deepEqualCanonical({ a: 1, b: undefined }, { a: 1 })).toBe(true);
    expect(deepEqualCanonical({ a: 1 }, { a: 1, b: undefined })).toBe(true);
  });

  it("treats null and undefined equivalently at top level", () => {
    expect(deepEqualCanonical(null, undefined)).toBe(true);
  });

  it("flags genuine structural differences", () => {
    expect(deepEqualCanonical({ a: 1 }, { a: 2 })).toBe(false);
    expect(deepEqualCanonical({ a: { b: 1 } }, { a: { b: 2 } })).toBe(false);
  });
});

describe("lockedFieldChange", () => {
  it("returns null when neither endpoint nor registry differs", () => {
    const spec = baseSpec({ endpoint: REMOTE_ENDPOINT, registry: "cloud" });
    const parsed = JSON.parse(JSON.stringify(spec));
    expect(lockedFieldChange(spec, parsed)).toBeNull();
  });

  it("returns null when only object key order differs (regression for #5)", () => {
    const spec = baseSpec({
      endpoint: {
        backend: "a2a",
        base_url: "https://remote.example.com",
        target: "remote-agent",
        timeout_ms: 60_000,
      },
    });
    // Same endpoint, keys re-ordered as a user editing Raw JSON would naturally do.
    const reordered: Record<string, unknown> = {
      ...spec,
      endpoint: {
        timeout_ms: 60_000,
        target: "remote-agent",
        base_url: "https://remote.example.com",
        backend: "a2a",
      },
    };
    expect(lockedFieldChange(spec, reordered)).toBeNull();
  });

  it("treats absent vs explicit null equivalently for endpoint", () => {
    const spec = baseSpec();
    expect(lockedFieldChange(spec, {})).toBeNull();
    expect(lockedFieldChange(spec, { endpoint: null })).toBeNull();
    expect(lockedFieldChange(spec, { endpoint: undefined })).toBeNull();
  });

  it("flags endpoint when the parsed value differs", () => {
    const spec = baseSpec({ endpoint: REMOTE_ENDPOINT });
    const parsed = {
      ...spec,
      endpoint: { ...REMOTE_ENDPOINT, base_url: "https://other.example.com" },
    };
    expect(lockedFieldChange(spec, parsed)).toBe("endpoint");
  });

  it("flags registry when the parsed value differs", () => {
    const spec = baseSpec({ registry: "cloud" });
    expect(lockedFieldChange(spec, { ...spec, registry: "local" })).toBe("registry");
  });

  it("flags endpoint before registry when both differ (stable order)", () => {
    const spec = baseSpec({ endpoint: REMOTE_ENDPOINT, registry: "cloud" });
    const parsed = {
      ...spec,
      endpoint: { ...REMOTE_ENDPOINT, base_url: "https://other.example.com" },
      registry: "local",
    };
    expect(lockedFieldChange(spec, parsed)).toBe("endpoint");
  });

  it("flags registry being removed (cloud -> absent)", () => {
    const spec = baseSpec({ registry: "cloud" });
    expect(lockedFieldChange(spec, {})).toBe("registry");
  });

  it("flags registry being introduced (absent -> cloud)", () => {
    const spec = baseSpec();
    expect(lockedFieldChange(spec, { registry: "cloud" })).toBe("registry");
  });
});

describe("diffPatchableAgentFields", () => {
  it("returns an empty patch when nothing changed", () => {
    const spec = baseSpec({
      context_policy: {
        max_context_tokens: 100,
        max_output_tokens: 10,
        min_recent_messages: 1,
        enable_prompt_cache: true,
      },
      active_hook_filter: ["permission"],
    });
    expect(diffPatchableAgentFields(spec, spec)).toEqual({});
  });

  it("returns an empty patch when only object key order differs", () => {
    // The original is what the server returned; the current is what the user
    // sees after re-formatting via Raw JSON. Same shape, different key order.
    const original = baseSpec({
      context_policy: {
        max_context_tokens: 100,
        max_output_tokens: 10,
        min_recent_messages: 1,
        enable_prompt_cache: true,
      },
      sections: { foo: { a: 1, b: 2 } },
    });
    const current: AgentSpec = {
      ...original,
      context_policy: {
        // Keys in different order.
        enable_prompt_cache: true,
        min_recent_messages: 1,
        max_output_tokens: 10,
        max_context_tokens: 100,
      },
      sections: { foo: { b: 2, a: 1 } },
    };
    expect(diffPatchableAgentFields(current, original)).toEqual({});
  });

  it("emits context_policy when it changed (regression for G1)", () => {
    const original = baseSpec();
    const current: AgentSpec = {
      ...original,
      context_policy: {
        max_context_tokens: 222_222,
        max_output_tokens: 16_384,
        min_recent_messages: 10,
        enable_prompt_cache: true,
      },
    };
    const patch = diffPatchableAgentFields(current, original);
    expect(patch).toHaveProperty("context_policy");
    expect(patch.context_policy).toEqual(current.context_policy);
  });

  it("emits active_hook_filter when it changed (regression for G1)", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = {
      ...original,
      active_hook_filter: ["permission"],
    };
    const patch = diffPatchableAgentFields(current, original);
    expect(patch).toEqual({ active_hook_filter: ["permission"] });
  });

  it("emits each patchable scalar when it changed", () => {
    const original = baseSpec();
    const current: AgentSpec = {
      ...original,
      model_id: "new-model",
      system_prompt: "new",
      max_rounds: 64,
      max_continuation_retries: 5,
      delegates: ["other-agent"],
      allowed_tools: ["Bash"],
      excluded_tools: ["Read"],
      reasoning_effort: "high",
      plugin_ids: ["permission", "reminder"],
      sections: { permission: { default_behavior: "deny", rules: [] } },
    };
    const patch = diffPatchableAgentFields(current, original);
    expect(Object.keys(patch).sort()).toEqual(
      [
        "allowed_tools",
        "delegates",
        "excluded_tools",
        "max_continuation_retries",
        "max_rounds",
        "model_id",
        "plugin_ids",
        "reasoning_effort",
        "sections",
        "system_prompt",
      ].sort(),
    );
  });

  it("does not emit locked or non-patchable fields even when they differ", () => {
    const original = baseSpec({ endpoint: REMOTE_ENDPOINT, registry: "cloud" });
    const current: AgentSpec = {
      ...original,
      endpoint: { ...REMOTE_ENDPOINT, base_url: "https://changed.example.com" },
      registry: "local",
      id: "renamed-id",
      created_at: 1,
      updated_at: 2,
    };
    expect(diffPatchableAgentFields(current, original)).toEqual({});
  });

  it("every G1 field is in PATCHABLE_AGENT_FIELDS", () => {
    // This list locks in the schema-level G1 fix and is the canonical answer
    // to "what does the customized PATCH path actually persist?".
    expect(PATCHABLE_AGENT_FIELDS).toContain("context_policy");
    expect(PATCHABLE_AGENT_FIELDS).toContain("active_hook_filter");
  });
});

describe("cloneAgentSpecForEditor", () => {
  it("clears id, created_at, updated_at, registry, and endpoint", () => {
    const source: AgentSpec = baseSpec({
      id: "ingest",
      created_at: 1_700_000_000_000,
      updated_at: 1_700_000_500_000,
      registry: "cloud",
      endpoint: REMOTE_ENDPOINT,
    });

    const cloned = cloneAgentSpecForEditor(source);
    expect(cloned.id).toBe("");
    expect(cloned.created_at).toBeUndefined();
    expect(cloned.updated_at).toBeUndefined();
    expect(cloned.registry).toBeUndefined();
    expect(cloned.endpoint).toBeUndefined();
  });

  it("preserves editable AgentSpec body (prompt, model_id, plugins, sections)", () => {
    const source: AgentSpec = baseSpec({
      id: "ingest",
      system_prompt: "Be helpful.",
      model_id: "research-default",
      plugin_ids: ["permission", "reminder"],
      active_hook_filter: ["permission"],
      sections: { ingest: { mode: "planning" } },
      endpoint: REMOTE_ENDPOINT,
      registry: "cloud",
    });

    const cloned = cloneAgentSpecForEditor(source);
    expect(cloned.system_prompt).toBe("Be helpful.");
    expect(cloned.model_id).toBe("research-default");
    expect(cloned.plugin_ids).toEqual(["permission", "reminder"]);
    expect(cloned.active_hook_filter).toEqual(["permission"]);
    expect(cloned.sections).toEqual({ ingest: { mode: "planning" } });
  });

  it("does not mutate the source spec", () => {
    const source: AgentSpec = baseSpec({
      id: "ingest",
      registry: "cloud",
      endpoint: REMOTE_ENDPOINT,
      created_at: 1,
      updated_at: 2,
    });
    const snapshot = JSON.parse(JSON.stringify(source));
    cloneAgentSpecForEditor(source);
    expect(source).toEqual(snapshot);
  });
});

describe("togglePluginState", () => {
  it("adds a plugin when not present", () => {
    expect(togglePluginState(["permission"], [], "reminder")).toEqual({
      plugin_ids: ["permission", "reminder"],
      active_hook_filter: [],
    });
  });

  it("removes a plugin when present", () => {
    expect(togglePluginState(["permission", "reminder"], [], "reminder")).toEqual({
      plugin_ids: ["permission"],
      active_hook_filter: [],
    });
  });

  it("prunes active_hook_filter when the plugin is removed", () => {
    expect(
      togglePluginState(["permission", "reminder"], ["permission", "reminder"], "reminder"),
    ).toEqual({
      plugin_ids: ["permission"],
      active_hook_filter: ["permission"],
    });
  });

  it("leaves active_hook_filter untouched when adding (no stale entry to prune)", () => {
    expect(togglePluginState(["permission"], ["permission"], "reminder")).toEqual({
      plugin_ids: ["permission", "reminder"],
      active_hook_filter: ["permission"],
    });
  });

  it("keeps active_hook_filter absent when it was absent and we add a plugin", () => {
    const result = togglePluginState(["permission"], undefined, "reminder");
    expect(result.plugin_ids).toEqual(["permission", "reminder"]);
    expect(result.active_hook_filter).toBeUndefined();
  });

  it("keeps active_hook_filter absent when removing a plugin (avoids stray empty array)", () => {
    const result = togglePluginState(["permission", "reminder"], undefined, "reminder");
    expect(result.plugin_ids).toEqual(["permission"]);
    expect(result.active_hook_filter).toBeUndefined();
  });

  it("treats undefined plugin_ids as empty", () => {
    expect(togglePluginState(undefined, undefined, "permission")).toEqual({
      plugin_ids: ["permission"],
      active_hook_filter: undefined,
    });
  });

  it("does not mutate the input arrays", () => {
    const plugins = ["permission", "reminder"];
    const filter = ["permission", "reminder"];
    const pluginsSnapshot = [...plugins];
    const filterSnapshot = [...filter];
    togglePluginState(plugins, filter, "reminder");
    expect(plugins).toEqual(pluginsSnapshot);
    expect(filter).toEqual(filterSnapshot);
  });
});

describe("partitionActiveHookFilter", () => {
  it("classifies all entries as active when every id is loaded", () => {
    expect(
      partitionActiveHookFilter(["permission", "reminder"], ["permission", "reminder"]),
    ).toEqual({ active: ["permission", "reminder"], stale: [] });
  });

  it("classifies entries with no matching plugin as stale", () => {
    expect(
      partitionActiveHookFilter(["permission", "ghost"], ["permission"]),
    ).toEqual({ active: ["permission"], stale: ["ghost"] });
  });

  it("treats undefined filter as no entries", () => {
    expect(partitionActiveHookFilter(undefined, ["permission"])).toEqual({
      active: [],
      stale: [],
    });
  });

  it("treats undefined plugin_ids as all-stale", () => {
    expect(partitionActiveHookFilter(["ghost"], undefined)).toEqual({
      active: [],
      stale: ["ghost"],
    });
  });

  it("de-duplicates within the result while preserving first-seen order", () => {
    expect(
      partitionActiveHookFilter(
        ["permission", "permission", "ghost", "ghost"],
        ["permission"],
      ),
    ).toEqual({ active: ["permission"], stale: ["ghost"] });
  });
});

describe("redactEndpointForDisplay", () => {
  it("masks bearer_token but keeps non-secret fields", () => {
    const endpoint: RemoteEndpoint = {
      backend: "a2a",
      base_url: "https://remote.example.com",
      target: "remote-agent",
      timeout_ms: 60_000,
      auth: {
        type: "bearer",
        bearer_token: "super-secret-token",
      },
    };
    const redacted = redactEndpointForDisplay(endpoint);
    expect(redacted.base_url).toBe("https://remote.example.com");
    expect(redacted.auth?.type).toBe("bearer");
    expect(redacted.auth?.bearer_token).toBe("***");
  });

  it("masks api_key / authorization / secret / password / passphrase / credential", () => {
    const endpoint: RemoteEndpoint = {
      base_url: "https://x",
      auth: {
        type: "custom",
        api_key: "k1",
        apiKey: "k2",
        authorization: "Bearer abc",
        secret: "s",
        password: "p",
        passphrase: "pp",
        credential: "c",
        client_secret: "cs",
        access_token: "at",
        refresh_token: "rt",
        id_token: "it",
        private_key: "pk",
        privateKey: "pk2",
      },
    };
    const redacted = redactEndpointForDisplay(endpoint);
    const auth = (redacted.auth ?? {}) as Record<string, unknown>;
    for (const key of [
      "api_key",
      "apiKey",
      "authorization",
      "secret",
      "password",
      "passphrase",
      "credential",
      "client_secret",
      "access_token",
      "refresh_token",
      "id_token",
      "private_key",
      "privateKey",
    ]) {
      expect(auth[key], `key ${key} should be redacted`).toBe("***");
    }
    expect(auth.type).toBe("custom");
  });

  it("redacts secret keys nested inside options", () => {
    const endpoint: RemoteEndpoint = {
      base_url: "https://x",
      options: {
        retry: 3,
        headers: { Authorization: "Bearer leaked", "X-Trace": "ok" },
      },
    };
    const redacted = redactEndpointForDisplay(endpoint);
    const headers = (redacted.options?.headers ?? {}) as Record<string, unknown>;
    expect(headers.Authorization).toBe("***");
    expect(headers["X-Trace"]).toBe("ok");
    expect(redacted.options?.retry).toBe(3);
  });

  it("preserves null / undefined values rather than replacing them with ***", () => {
    const endpoint: RemoteEndpoint = {
      base_url: "https://x",
      auth: {
        type: "none",
        bearer_token: null as unknown as string,
      },
    };
    const redacted = redactEndpointForDisplay(endpoint);
    expect(redacted.auth?.bearer_token).toBeNull();
  });

  it("does not mutate the source endpoint", () => {
    const endpoint: RemoteEndpoint = {
      base_url: "https://x",
      auth: { type: "bearer", bearer_token: "real-secret" },
    };
    const snapshot = JSON.parse(JSON.stringify(endpoint));
    redactEndpointForDisplay(endpoint);
    expect(endpoint).toEqual(snapshot);
  });

  it("is case-insensitive on key matching (Authorization, BearerToken, etc.)", () => {
    const endpoint = {
      base_url: "https://x",
      auth: { type: "bearer", BearerToken: "abc", AUTHORIZATION: "z" },
    } as unknown as RemoteEndpoint;
    const redacted = redactEndpointForDisplay(endpoint);
    const auth = redacted.auth as Record<string, unknown>;
    expect(auth.BearerToken).toBe("***");
    expect(auth.AUTHORIZATION).toBe("***");
  });
});

describe("redactAgentSpecForDisplay (R3 #1)", () => {
  it("returns the spec unchanged when no endpoint is set", () => {
    const spec = baseSpec();
    expect(redactAgentSpecForDisplay(spec)).toBe(spec);
  });

  it("redacts secret keys inside endpoint while preserving everything else", () => {
    const spec = baseSpec({
      endpoint: {
        backend: "a2a",
        base_url: "https://remote.example.com",
        auth: { type: "bearer", bearer_token: "real-secret" },
      },
    });
    const redacted = redactAgentSpecForDisplay(spec);
    expect(redacted.id).toBe(spec.id);
    expect(redacted.system_prompt).toBe(spec.system_prompt);
    expect(redacted.endpoint?.base_url).toBe("https://remote.example.com");
    expect(redacted.endpoint?.auth?.bearer_token).toBe("***");
  });

  it("does not mutate the source spec", () => {
    const spec = baseSpec({
      endpoint: { base_url: "https://x", auth: { type: "bearer", bearer_token: "abc" } },
    });
    const snapshot = JSON.parse(JSON.stringify(spec));
    redactAgentSpecForDisplay(spec);
    expect(spec).toEqual(snapshot);
  });
});

describe("ALLOWED_AGENT_FIELDS / unknownAgentSpecFields (R3 #3)", () => {
  it("includes identity, locked, and patchable fields", () => {
    expect(ALLOWED_AGENT_FIELDS).toEqual(
      expect.arrayContaining([
        "id",
        "created_at",
        "updated_at",
        "endpoint",
        "registry",
        ...PATCHABLE_AGENT_FIELDS,
      ]),
    );
  });

  it("returns an empty list for a fully-known spec object", () => {
    const parsed: Record<string, unknown> = {
      id: "a",
      model_id: "m",
      system_prompt: "p",
      plugin_ids: ["permission"],
      sections: {},
      delegates: [],
    };
    expect(unknownAgentSpecFields(parsed)).toEqual([]);
  });

  it("flags top-level keys outside the allowlist", () => {
    const parsed: Record<string, unknown> = {
      id: "a",
      model_id: "m",
      system_prompt: "p",
      surprise: 1,
      future_field: { nested: true },
    };
    expect(unknownAgentSpecFields(parsed).sort()).toEqual(["future_field", "surprise"]);
  });
});

describe("mergeLockedFields (R3 #1)", () => {
  it("overlays current spec's endpoint and registry onto parsed", () => {
    const spec = baseSpec({
      endpoint: { base_url: "https://real.example.com", auth: { type: "bearer", bearer_token: "real" } },
      registry: "cloud",
    });
    const parsed: Record<string, unknown> = {
      ...spec,
      endpoint: { base_url: "https://real.example.com", auth: { type: "bearer", bearer_token: "***" } },
      registry: "cloud",
    };
    const merged = mergeLockedFields(parsed, spec);
    expect(merged.endpoint).toEqual(spec.endpoint);
    expect(merged.registry).toBe("cloud");
  });

  it("removes locked keys from parsed when the current spec has them absent", () => {
    const spec = baseSpec();
    const parsed: Record<string, unknown> = {
      ...spec,
      endpoint: { base_url: "ghost" },
      registry: "drift",
    };
    const merged = mergeLockedFields(parsed, spec);
    expect("endpoint" in merged).toBe(false);
    expect("registry" in merged).toBe(false);
  });

  it("does not mutate inputs", () => {
    const spec = baseSpec({ endpoint: { base_url: "https://x" } });
    const parsed: Record<string, unknown> = { id: "p", endpoint: { base_url: "y" } };
    const specSnap = JSON.parse(JSON.stringify(spec));
    const parsedSnap = JSON.parse(JSON.stringify(parsed));
    mergeLockedFields(parsed, spec);
    expect(spec).toEqual(specSnap);
    expect(parsed).toEqual(parsedSnap);
  });
});

describe("active_hook_filter [] vs absent normalization (R3 #2)", () => {
  it("does not emit a patch when active_hook_filter goes from absent to []", () => {
    const original = baseSpec({ active_hook_filter: undefined });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    expect(diffPatchableAgentFields(current, original)).toEqual({});
  });

  it("does not emit a patch when active_hook_filter goes from [] to absent", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({});
  });

  it("still emits the actual array when an entry is added", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: ["permission"] };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      active_hook_filter: ["permission"],
    });
  });
});
