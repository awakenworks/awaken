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
  redactSecretString,
  redactSecretsForDisplay,
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

  // Contract pin (R14): for a locked field whose current spec value is
  // "no value", the three Raw JSON writings { absent, null, undefined }
  // are equivalent. Re-typing `endpoint: null` to "make the absence
  // explicit" must be a no-op, not a "locked field changed" error.
  // This is intentional normalization, not a silent drop — see the
  // JSDoc on `lockedFieldChange` for the full rationale. Position B
  // (presence-aware) was considered and rejected because it produces
  // user-visible errors for semantically-equivalent edits.
  it("locked-field normalization: absent ≡ explicit null ≡ explicit undefined", () => {
    const spec = baseSpec();
    expect(lockedFieldChange(spec, {})).toBeNull();
    expect(lockedFieldChange(spec, { endpoint: null })).toBeNull();
    expect(lockedFieldChange(spec, { endpoint: undefined })).toBeNull();
    expect(lockedFieldChange(spec, { registry: null })).toBeNull();
    expect(lockedFieldChange(spec, { registry: undefined })).toBeNull();
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
  const EMPTY_PLAN = { patch: {}, clear: [] };

  it("returns an empty plan when nothing changed", () => {
    const spec = baseSpec({
      context_policy: {
        max_context_tokens: 100,
        max_output_tokens: 10,
        min_recent_messages: 1,
        enable_prompt_cache: true,
      },
      active_hook_filter: ["permission"],
    });
    expect(diffPatchableAgentFields(spec, spec)).toEqual(EMPTY_PLAN);
  });

  it("returns an empty plan when only object key order differs", () => {
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
    expect(diffPatchableAgentFields(current, original)).toEqual(EMPTY_PLAN);
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
    const plan = diffPatchableAgentFields(current, original);
    expect(plan.patch).toHaveProperty("context_policy");
    expect(plan.patch.context_policy).toEqual(current.context_policy);
    expect(plan.clear).toEqual([]);
  });

  it("emits active_hook_filter when it changed (regression for G1)", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = {
      ...original,
      active_hook_filter: ["permission"],
    };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan.patch).toEqual({ active_hook_filter: ["permission"] });
    expect(plan.clear).toEqual([]);
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
    const plan = diffPatchableAgentFields(current, original);
    expect(Object.keys(plan.patch).sort()).toEqual(
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
    expect(plan.clear).toEqual([]);
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
    expect(diffPatchableAgentFields(current, original)).toEqual(EMPTY_PLAN);
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

  // R10 #6 — Removing the LAST entry from a previously-non-empty
  // active_hook_filter must collapse the field back to `undefined`,
  // not leave it as a stray `[]`. Empty array means "all hooks run"
  // (same as absent), but the dirty-tracking / patch-diff path can't
  // distinguish between explicit-empty and "user just removed the
  // last entry"; emitting `[]` would surface as a no-op override or a
  // perpetual dirty draft.
  it("collapses active_hook_filter to undefined when the prune empties it", () => {
    const result = togglePluginState(["reminder"], ["reminder"], "reminder");
    expect(result.plugin_ids).toEqual([]);
    expect(result.active_hook_filter).toBeUndefined();
  });

  it("keeps active_hook_filter as `[]` if it was already empty before removal", () => {
    // Distinguishes "user removed last filter entry just now" (collapse
    // to undefined) from "filter was always explicit-empty" (preserve
    // `[]` so we don't change shape under the user's feet).
    const result = togglePluginState(["reminder"], [], "reminder");
    expect(result.plugin_ids).toEqual([]);
    expect(result.active_hook_filter).toEqual([]);
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

describe("redactSecretsForDisplay (R6 #B/#C — generic deep redact)", () => {
  it("masks secret keys inside an arbitrary audit `before` snapshot", () => {
    // Audit events carry the full pre-mutation spec; they can include
    // `endpoint.auth.bearer_token`. The deep redactor walks the whole
    // tree, not just `endpoint` — so a future schema that nests
    // credentials elsewhere also gets masked.
    const before = {
      id: "alpha",
      endpoint: {
        base_url: "https://remote",
        auth: { type: "bearer", bearer_token: "leaked" },
      },
      sections: { permission: { rules: [] } },
    };
    const redacted = redactSecretsForDisplay(before);
    expect(redacted.endpoint.auth.bearer_token).toBe("***");
    expect(redacted.endpoint.base_url).toBe("https://remote");
    expect(redacted.id).toBe("alpha");
    // No accidental side-mutation.
    expect(before.endpoint.auth.bearer_token).toBe("leaked");
  });

  it("masks credentials nested inside a trace event payload", () => {
    const event = {
      kind: "agent_resolved",
      ts: 1_000,
      payload: {
        agent_id: "x",
        endpoint: { auth: { api_key: "key-leaked", type: "bearer" } },
      },
    };
    const redacted = redactSecretsForDisplay(event);
    expect((redacted.payload.endpoint.auth as Record<string, unknown>).api_key).toBe("***");
  });

  it("passes through primitives and arrays unchanged", () => {
    expect(redactSecretsForDisplay(42)).toBe(42);
    expect(redactSecretsForDisplay("hello")).toBe("hello");
    expect(redactSecretsForDisplay(null)).toBeNull();
    expect(redactSecretsForDisplay(undefined)).toBeUndefined();
    expect(redactSecretsForDisplay([1, 2, 3])).toEqual([1, 2, 3]);
  });

  // R10 #4 — Outside the `auth` default-deny context, the redactor
  // relied purely on the pattern list and missed HTTP-flavored secret
  // keys. Tool outputs, trace event payloads, and audit-log diffs can
  // surface raw `headers.cookie` / `set-cookie` / `jwt` / `bearer` /
  // `session_*` shapes without any `auth` wrapper.
  it("masks cookie / set-cookie / jwt / bearer / session keys anywhere in the payload", () => {
    const payload = {
      response: {
        headers: {
          cookie: "sid=abc",
          "set-cookie": "session=xyz",
          "user-agent": "preserved",
        },
        body: {
          jwt: "raw-jwt",
          bearer: "raw-bearer",
          session_id: "raw-session",
          session: "raw-session",
        },
      },
    };
    const redacted = redactSecretsForDisplay(payload);
    const headers = redacted.response.headers as Record<string, unknown>;
    expect(headers.cookie).toBe("***");
    expect(headers["set-cookie"]).toBe("***");
    // Non-sensitive keys stay through.
    expect(headers["user-agent"]).toBe("preserved");
    const body = redacted.response.body as Record<string, unknown>;
    expect(body.jwt).toBe("***");
    expect(body.bearer).toBe("***");
    expect(body.session).toBe("***");
    expect(body.session_id).toBe("***");
  });

  it("masks access_key / accesskey in arbitrary contexts", () => {
    const payload = {
      adapter_options: { access_key: "raw-AK", accesskey: "raw-AK-alt" },
    };
    const redacted = redactSecretsForDisplay(payload);
    const opts = redacted.adapter_options as Record<string, unknown>;
    expect(opts.access_key).toBe("***");
    expect(opts.accesskey).toBe("***");
  });

  // R8 #2 — `RemoteAuth` allows arbitrary keys, so a credential under
  // `jwt` / `cookie` / `session` / `x-api-key` / `header` / `bearer` etc.
  // would slip past the pattern list. Generic redact must apply
  // default-deny when it recurses into an `auth` object so audit log,
  // trace drawer, and DiffModal all match `redactEndpointForDisplay`'s
  // strength.
  it("default-denies non-pattern keys when recursing into an `auth` object", () => {
    const event = {
      payload: {
        endpoint: {
          auth: {
            type: "bearer",
            jwt: "raw-jwt-leaked",
            cookie: "sid=abc",
            session: "session-leaked",
            "x-api-key": "raw-key",
            header: "raw-header",
            // Standalone `bearer` (no `_token` suffix) — pattern alone
            // would miss this; default-deny covers it.
            bearer: "raw-bearer-leaked",
          },
        },
      },
    };
    const redacted = redactSecretsForDisplay(event);
    const auth = redacted.payload.endpoint.auth as Record<string, unknown>;
    expect(auth.type).toBe("bearer");
    expect(auth.jwt).toBe("***");
    expect(auth.cookie).toBe("***");
    expect(auth.session).toBe("***");
    expect(auth["x-api-key"]).toBe("***");
    expect(auth.header).toBe("***");
    expect(auth.bearer).toBe("***");
  });

  it("default-denies even when `auth` lives several levels deep in a trace span", () => {
    const span = {
      trace: {
        context: {
          attributes: {
            endpoint: { auth: { type: "custom", weird_key: "leak" } },
          },
        },
      },
    };
    const redacted = redactSecretsForDisplay(span);
    expect(
      (redacted.trace.context.attributes.endpoint.auth as Record<string, unknown>).weird_key,
    ).toBe("***");
  });

  // R13 — Key-based redaction misses raw strings whose key isn't itself
  // a credential pattern. Trace event payloads, audit `before`/`after`
  // snapshots, and DiffModal stringify untrusted blobs as-is — a string
  // field named `output` / `body` / `message` can carry `Authorization:`
  // header lines, inline `Bearer …` tokens, JWTs, or `sk-…` keys. The
  // generic redactor must apply `redactSecretString` to every primitive
  // string so all display paths share one defensive layer.
  it("masks secret-bearing patterns inside primitive string values", () => {
    const event = {
      kind: "tool_output",
      payload: {
        output: "Authorization: Bearer sk-real-secret-value-1234567890abcdef",
        notes: "regular log line — should stay untouched",
      },
    };
    const redacted = redactSecretsForDisplay(event);
    const payload = redacted.payload as Record<string, unknown>;
    expect(payload.output).not.toContain("sk-real-secret-value");
    expect(payload.output).toContain("Authorization: ***");
    expect(payload.notes).toBe("regular log line — should stay untouched");
  });

  it("redacts secret strings inside arrays of primitives", () => {
    const event = {
      payload: {
        lines: [
          "first line",
          "Bearer abc123def456ghi789jkl",
          "Cookie: session=raw-session-id",
        ],
      },
    };
    const redacted = redactSecretsForDisplay(event);
    const lines = (redacted.payload as Record<string, unknown>).lines as string[];
    expect(lines[0]).toBe("first line");
    expect(lines[1]).toContain("Bearer ***");
    expect(lines[1]).not.toContain("abc123def456ghi789jkl");
    expect(lines[2]).toContain("Cookie: ***");
    expect(lines[2]).not.toContain("raw-session-id");
  });

  it("redacts a top-level primitive string (passes through redactSecretString)", () => {
    expect(redactSecretsForDisplay("Authorization: Bearer raw-token-1234567890")).toBe(
      "Authorization: ***",
    );
  });

  it("preserves null / undefined inside `auth` as semantic markers", () => {
    const value = {
      endpoint: {
        auth: { type: "bearer", weird_key: null, other: undefined },
      },
    };
    const redacted = redactSecretsForDisplay(value);
    const auth = redacted.endpoint.auth as Record<string, unknown>;
    expect(auth.type).toBe("bearer");
    expect(auth.weird_key).toBeNull();
    expect(auth.other).toBeUndefined();
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

  // R8 #6 — Drift guard: `ALLOWED_AGENT_FIELDS` is hand-maintained and the
  // editor's Raw JSON Apply path rejects any top-level key not in this
  // list. When `types.ts` (which mirrors the Rust `AgentSpec`) gains a
  // new key, the allowlist must grow in lockstep — otherwise a legal
  // wire-shape gets rejected as "unknown field". This assertion fails
  // type-checking the moment a key appears on `AgentSpec` that isn't in
  // `ALLOWED_AGENT_FIELDS`.
  it("compile-time: ALLOWED_AGENT_FIELDS covers every AgentSpec key", () => {
    type Known = (typeof ALLOWED_AGENT_FIELDS)[number];
    type MissingKeys = Exclude<keyof AgentSpec, Known>;
    // If `MissingKeys` resolves to anything other than `never`, this
    // assignment fails the typecheck — surfacing the drift in CI.
    const _check: MissingKeys extends never ? true : never = true;
    expect(_check).toBe(true);
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
  const EMPTY_PLAN = { patch: {}, clear: [] };
  it("does not emit a patch when active_hook_filter goes from absent to []", () => {
    const original = baseSpec({ active_hook_filter: undefined });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    expect(diffPatchableAgentFields(current, original)).toEqual(EMPTY_PLAN);
  });

  it("does not emit a patch when active_hook_filter goes from [] to absent", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual(EMPTY_PLAN);
  });

  it("still emits the actual array when an entry is added", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: ["permission"] };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: { active_hook_filter: ["permission"] },
      clear: [],
    });
  });
});

describe("active_hook_filter override semantics (R13)", () => {
  // Per `agent_spec_patch.rs::merge_patch`, the override layer for
  // `active_hook_filter` is `patch.unwrap_or(base)` — i.e. absent override
  // means "inherit base" and `Some([])` means "no filter, override base".
  // The runtime engine (`phase/engine.rs::filter_hooks`) then treats the
  // resolved set: `is_empty() || contains(plugin_id)` ⇒ empty means all
  // hooks run. So an "All plugins" intent on a customized record with a
  // non-empty base filter MUST land as `patch: { active_hook_filter: [] }`
  // — a CLEAR would inherit the base filter, reversing the user's choice.
  it("absent → absent: no diff", () => {
    const original = baseSpec({ active_hook_filter: undefined });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({ patch: {}, clear: [] });
  });

  it("[] → []: no diff", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    expect(diffPatchableAgentFields(current, original)).toEqual({ patch: {}, clear: [] });
  });

  it("absent → []: no diff (wire-equivalent)", () => {
    const original = baseSpec({ active_hook_filter: undefined });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    expect(diffPatchableAgentFields(current, original)).toEqual({ patch: {}, clear: [] });
  });

  it("[] → absent: no diff (wire-equivalent)", () => {
    const original = baseSpec({ active_hook_filter: [] });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({ patch: {}, clear: [] });
  });

  it("['permission'] → ['permission','reminder']: patches the new array", () => {
    const original = baseSpec({ active_hook_filter: ["permission"] });
    const current: AgentSpec = {
      ...original,
      active_hook_filter: ["permission", "reminder"],
    };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: { active_hook_filter: ["permission", "reminder"] },
      clear: [],
    });
  });

  it("['permission'] → absent: emits patch [] (override base 'All plugins')", () => {
    // User had a saved filter, then clicked "All plugins" — the UI emits
    // `undefined` (or `[]`). The diff MUST emit `patch: []` so the
    // override flips the customized record's filter to empty regardless
    // of base. Emitting CLEAR here would inherit base's filter (if any)
    // and silently undo the user's intent.
    const original = baseSpec({ active_hook_filter: ["permission"] });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: { active_hook_filter: [] },
      clear: [],
    });
  });

  it("['permission'] → []: emits patch [] (override base 'All plugins')", () => {
    const original = baseSpec({ active_hook_filter: ["permission"] });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: { active_hook_filter: [] },
      clear: [],
    });
  });
});

describe("Raw JSON locked-field check ordering (R5 #1)", () => {
  // The Raw JSON `handleApply` must compare the parsed payload against
  // the REDACTED display spec FIRST, then overlay real locked-field
  // values. If the order is reversed (overlay first, then compare),
  // `mergeLockedFields` overwrites `parsed.endpoint` with `spec.endpoint`
  // and the subsequent `lockedFieldChange` always reads "equal" — user
  // edits to `endpoint.base_url` / `endpoint.auth.*` / `registry` get
  // silently dropped on Apply success.
  it("display-spec lockedFieldChange catches base_url edits the buggy order would miss", () => {
    const spec = baseSpec({
      endpoint: {
        base_url: "https://real.example.com",
        auth: { type: "bearer", bearer_token: "real-secret" },
      },
    });
    const displaySpec = redactAgentSpecForDisplay(spec);
    // Scenario: user changed `endpoint.base_url` in Raw JSON. The
    // textarea showed `***` for the bearer token; the user kept that
    // placeholder but flipped the URL.
    const parsed = {
      ...spec,
      endpoint: {
        base_url: "https://evil.example.com",
        auth: { type: "bearer", bearer_token: "***" },
      },
    } as Record<string, unknown>;

    // The CORRECT order — compare display vs parsed before any merge —
    // catches the edit.
    expect(lockedFieldChange(displaySpec, parsed)).toBe("endpoint");

    // The BUGGY order — overlay first, then compare against the
    // overlaid payload — silently accepts the edit. This assertion
    // documents the regression vector so a future refactor that
    // re-introduces the merge-then-compare order trips this test.
    const buggyMerged = mergeLockedFields(parsed, spec);
    expect(lockedFieldChange(spec, buggyMerged)).toBeNull();
  });

  it("display-spec compare allows the `***` placeholder to survive (no false positive)", () => {
    const spec = baseSpec({
      endpoint: {
        base_url: "https://real.example.com",
        auth: { type: "bearer", bearer_token: "real-secret" },
      },
    });
    const displaySpec = redactAgentSpecForDisplay(spec);
    // The textarea was seeded from `displaySpec`. The user touched
    // nothing in `endpoint` so `parsed.endpoint` == `displaySpec.endpoint`.
    const parsed = JSON.parse(JSON.stringify(displaySpec)) as Record<string, unknown>;
    expect(lockedFieldChange(displaySpec, parsed)).toBeNull();
  });

  it("flags registry changes the same way endpoint changes are flagged", () => {
    const spec = baseSpec({ registry: "cloud" });
    const displaySpec = redactAgentSpecForDisplay(spec);
    const parsed = { ...spec, registry: "evil-registry" } as Record<string, unknown>;
    expect(lockedFieldChange(displaySpec, parsed)).toBe("registry");
  });

  it("flags endpoint removal (locked field deletion is still a locked change)", () => {
    const spec = baseSpec({ endpoint: { base_url: "https://x" } });
    const displaySpec = redactAgentSpecForDisplay(spec);
    const parsed = JSON.parse(JSON.stringify(displaySpec)) as Record<string, unknown>;
    delete parsed.endpoint;
    expect(lockedFieldChange(displaySpec, parsed)).toBe("endpoint");
  });
});

describe("clear vs patch routing (R6 #A — supersedes R5 #2)", () => {
  // R5 emitted `null` in the patch when a field went to undefined. That
  // prevented data loss (JSON.stringify no longer dropped the key) but
  // left an explicit-null override behind for nullable fields, so the
  // "customized" badge stuck on even after a revert. R6 splits the diff
  // into `patch` (upserts, including explicit nulls the user really set)
  // and `clear` (fields the Save flow will DELETE per-field) — this is
  // the semantic the backend's `clearAgentOverrideField` endpoint exists
  // to express.
  it("routes a cleared `reasoning_effort` to the clear list (revert override)", () => {
    const original = baseSpec({ reasoning_effort: "high" });
    const current: AgentSpec = { ...original, reasoning_effort: undefined };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: {}, clear: ["reasoning_effort"] });
  });

  // R13 — `active_hook_filter` carries an `All plugins` semantic that CLEAR
  // cannot express. On a customized record whose base has
  // `active_hook_filter: ["permission"]`, emitting CLEAR would delete the
  // override and inherit the base's filter — the opposite of what a user
  // who just clicked "All plugins" intended. Emit `patch: []` instead so
  // the override layer turns filtering off regardless of base. See also
  // the dedicated "active_hook_filter override semantics" describe block
  // below.
  it("routes turning off filtering for `active_hook_filter` to patch [] (not clear)", () => {
    const original = baseSpec({ active_hook_filter: ["permission", "reminder"] });
    const current: AgentSpec = { ...original, active_hook_filter: undefined };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: { active_hook_filter: [] }, clear: [] });
  });

  it("routes plugin_ids removal to the clear list", () => {
    const original = baseSpec({ plugin_ids: ["permission"] });
    const current: AgentSpec = { ...original, plugin_ids: undefined };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: {}, clear: ["plugin_ids"] });
  });

  it("routes allowed_tools revert to the clear list (nullable Option)", () => {
    const original = baseSpec({ allowed_tools: ["Bash"] });
    const current: AgentSpec = { ...original, allowed_tools: undefined };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: {}, clear: ["allowed_tools"] });
  });

  it("preserves explicit null in the patch when the user disabled a nullable field", () => {
    // Toggling "Apply custom policy" off sets the value to `null` (not
    // `undefined`). That's "explicit null override", semantically distinct
    // from "revert" — the user is saying "this agent has no context
    // policy even if the base had one". Route to patch, not clear.
    const original = baseSpec({ context_policy: { max_context_tokens: 1000 } as never });
    const current: AgentSpec = { ...original, context_policy: null };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: { context_policy: null }, clear: [] });
  });

  it("turning off filtering via empty array also routes to patch [] (not clear)", () => {
    const original = baseSpec({ active_hook_filter: ["permission"] });
    const current: AgentSpec = { ...original, active_hook_filter: [] };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: { active_hook_filter: [] }, clear: [] });
  });

  it("the full revert scenario routes every cleared field to the clear list", () => {
    // `active_hook_filter` is intentionally absent from this scenario —
    // its override semantics are tested separately (turning filtering off
    // becomes `patch: []`, not clear). Every other patchable field still
    // follows the clear-on-undefined rule.
    const original = baseSpec({
      plugin_ids: ["permission"],
      delegates: ["other-agent"],
      allowed_tools: ["Bash"],
      excluded_tools: ["Read"],
      reasoning_effort: "high",
    });
    const current: AgentSpec = {
      ...original,
      plugin_ids: undefined,
      delegates: undefined,
      allowed_tools: undefined,
      excluded_tools: undefined,
      reasoning_effort: undefined,
    };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan.patch).toEqual({});
    expect(plan.clear.sort()).toEqual(
      ["plugin_ids", "delegates", "allowed_tools", "excluded_tools", "reasoning_effort"].sort(),
    );
  });

  // R13 — When a saved customized agent has an explicit `null` override
  // (e.g. `context_policy: null` meaning "no context policy even if base
  // has one"), and the user deletes the field from Raw JSON, the draft
  // value becomes `undefined`. canonical equality maps both `null` and
  // `undefined` to `null`, so an order of "equal? then clear-if-undefined"
  // mis-classifies this as a no-op — neither emitting a CLEAR nor patching
  // — and the saved override stays as explicit-null forever. Clear must
  // be detected BEFORE canonical equality.
  it("routes explicit null -> undefined to clear (context_policy)", () => {
    const original = baseSpec({ context_policy: null });
    const current: AgentSpec = { ...original, context_policy: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: {},
      clear: ["context_policy"],
    });
  });

  it("routes explicit null -> undefined to clear (allowed_tools)", () => {
    const original = baseSpec({ allowed_tools: null });
    const current: AgentSpec = { ...original, allowed_tools: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: {},
      clear: ["allowed_tools"],
    });
  });

  it("routes explicit null -> undefined to clear (excluded_tools)", () => {
    const original = baseSpec({ excluded_tools: null });
    const current: AgentSpec = { ...original, excluded_tools: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: {},
      clear: ["excluded_tools"],
    });
  });

  it("routes explicit null -> undefined to clear (reasoning_effort)", () => {
    const original = baseSpec({ reasoning_effort: null });
    const current: AgentSpec = { ...original, reasoning_effort: undefined };
    expect(diffPatchableAgentFields(current, original)).toEqual({
      patch: {},
      clear: ["reasoning_effort"],
    });
  });

  it("does not put unchanged scalars in either bucket", () => {
    const spec = baseSpec({ model_id: "same" });
    const plan = diffPatchableAgentFields(spec, spec);
    expect(plan).toEqual({ patch: {}, clear: [] });
  });

  it("routes a value change to patch, not clear", () => {
    const original = baseSpec({ model_id: "old" });
    const current: AgentSpec = { ...original, model_id: "new" };
    const plan = diffPatchableAgentFields(current, original);
    expect(plan).toEqual({ patch: { model_id: "new" }, clear: [] });
  });
});

// R12 #3 — Pattern-based string redaction for sandbox tool output /
// errorText (paths where the key-based redactor doesn't apply because
// the payload is a raw string, not a structured object).
describe("redactSecretString (R12 #3)", () => {
  it("masks `Authorization: <value>` lines wholesale", () => {
    const out = redactSecretString("Authorization: Bearer sk-real-secret-value");
    expect(out).not.toContain("sk-real-secret-value");
    expect(out).toContain("Authorization: ***");
  });

  it("masks inline Bearer tokens", () => {
    const out = redactSecretString('called with Bearer abc123def456ghi789jkl');
    expect(out).toContain("Bearer ***");
    expect(out).not.toContain("abc123def456ghi789jkl");
  });

  it("masks Cookie / Set-Cookie header lines", () => {
    const a = redactSecretString("Cookie: session=raw-session-id");
    expect(a).not.toContain("raw-session-id");
    expect(a).toContain("Cookie: ***");

    const b = redactSecretString("Set-Cookie: sid=raw-sid; Path=/");
    expect(b).not.toContain("raw-sid");
    expect(b).toContain("Set-Cookie: ***");
  });

  it("masks key=value pairs for common credential field names", () => {
    const out = redactSecretString(
      "request: api_key=sk-test access_token=xyz123 password=hunter2 jwt=eyJabc",
    );
    expect(out).not.toContain("sk-test");
    expect(out).not.toContain("xyz123");
    expect(out).not.toContain("hunter2");
    expect(out).toContain("api_key=***");
    expect(out).toContain("access_token=***");
    expect(out).toContain("password=***");
    // `jwt=...` matched by key-pair (not the JWT regex since `eyJabc`
    // alone isn't a full JWT).
    expect(out).toContain("jwt=***");
  });

  it("masks JWT-shaped tokens", () => {
    const jwt =
      "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    const out = redactSecretString(`got token ${jwt} from auth`);
    expect(out).not.toContain(jwt);
    expect(out).toContain("***");
  });

  it("masks OpenAI-style sk- and Stripe-style sk_ keys", () => {
    const out = redactSecretString(
      "key1=sk-abcdefghijklmnop1234567890 key2=sk_live_abc123XYZ",
    );
    expect(out).not.toContain("sk-abcdefghijklmnop1234567890");
    expect(out).not.toContain("sk_live_abc123XYZ");
  });

  it("returns the input unchanged when no credential pattern matches", () => {
    expect(redactSecretString("regular tool output, nothing secret")).toBe(
      "regular tool output, nothing secret",
    );
  });

  it("handles empty / non-string input defensively", () => {
    expect(redactSecretString("")).toBe("");
    // Defensive — the helper is called from formatJson which already
    // narrows to string, but defensive coding pays off if callers drift.
    expect(redactSecretString(undefined as unknown as string)).toBe(
      undefined as unknown as string,
    );
  });
});
