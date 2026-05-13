/// Pure helpers used by `AgentEditorPage`. They are factored out here so they
/// stay unit-testable without having to drive React Router + the page state
/// machine, and so the JSON-editor / plugin-toggle / clone / patch-diff /
/// remote-endpoint-display code paths share one canonical implementation.
import type { AgentSpec, RemoteEndpoint } from "./api/types";

/**
 * Fields the editor cannot patch on a customized record:
 *  - `endpoint`   — remote-endpoint provenance, not exposed by any form.
 *  - `registry`   — runtime-locality marker; cloned records always become
 *                   locally defined and the editor never edits this in place.
 *
 * The Raw JSON Apply path consults this list so a draft can't silently
 * drift away from what Save can actually persist.
 */
export const LOCKED_AGENT_FIELDS = ["endpoint", "registry"] as const;
export type LockedAgentField = (typeof LOCKED_AGENT_FIELDS)[number];

/**
 * Fields that can be PATCHed onto a builtin / customized record via
 * `PATCH /v1/config/agents/:id/overrides`. `id` and `created_at` / `updated_at`
 * are not user-editable; `endpoint` and `registry` are locked (see above).
 *
 * Kept in sync with the AgentSpec PATCH whitelist server-side. Every field
 * the editor exposes in any form must appear here, otherwise edits to it
 * silently drop on the customized save path (this is the G1 data-loss bug).
 */
export const PATCHABLE_AGENT_FIELDS: Array<keyof AgentSpec> = [
  "model_id",
  "system_prompt",
  "max_rounds",
  "max_continuation_retries",
  "context_policy",
  "plugin_ids",
  "active_hook_filter",
  "sections",
  "allowed_tools",
  "excluded_tools",
  "delegates",
  "reasoning_effort",
];

/**
 * Stable JSON encoding: deep-sorts object keys so structurally-equal values
 * always serialize to the same string. Used as a building block for
 * `deepEqualCanonical` so re-formatting / key-reordering of Raw JSON doesn't
 * read as a locked-field change or trigger a spurious PATCH entry.
 *
 * Arrays preserve order (semantically significant). `undefined` collapses to
 * `null` so `{a: undefined}` and `{}` are treated equivalently — JSON has no
 * `undefined` and field absence vs explicit-undefined should not surface as
 * a diff in this editor.
 */
export function canonicalStringify(value: unknown): string {
  return JSON.stringify(canonicalize(value));
}

function canonicalize(value: unknown): unknown {
  if (value === undefined || value === null) return null;
  if (Array.isArray(value)) return value.map(canonicalize);
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, item]) => item !== undefined)
      .map(([key, item]) => [key, canonicalize(item)] as const)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return Object.fromEntries(entries);
  }
  return value;
}

/** Structural equality using `canonicalStringify` — order-insensitive for
 *  object keys, order-sensitive for arrays. */
export function deepEqualCanonical(a: unknown, b: unknown): boolean {
  return canonicalStringify(a) === canonicalStringify(b);
}

/**
 * Returns the name of the first locked field (`endpoint` or `registry`)
 * whose value would change if `parsed` were applied, or `null` when both
 * fields are unchanged. Uses canonical (key-order-insensitive) deep equality
 * so that re-indenting / reordering keys in Raw JSON doesn't falsely flag a
 * locked-field change.
 */
export function lockedFieldChange(
  spec: AgentSpec,
  parsed: Record<string, unknown>,
): LockedAgentField | null {
  for (const field of LOCKED_AGENT_FIELDS) {
    if (!deepEqualCanonical(spec[field], parsed[field])) {
      return field;
    }
  }
  return null;
}

/**
 * Normalize a patchable field value before comparison so wire-equivalent
 * shapes don't read as a change. The only special case today is
 * `active_hook_filter`: the Rust side marks it `skip_serializing_if =
 * "HashSet::is_empty"`, so absent and `[]` round-trip identically. Without
 * this, toggling the UI to "All" once would flip the spec from absent to
 * `[]` and the customized PATCH path would emit a no-op `active_hook_filter:
 * []` override.
 */
function normalizeForDiff(key: keyof AgentSpec, value: unknown): unknown {
  if (key === "active_hook_filter" && Array.isArray(value) && value.length === 0) {
    return undefined;
  }
  return value;
}

/**
 * Build the PATCH payload for a builtin / customized record save. Walks
 * `PATCHABLE_AGENT_FIELDS` and emits any field whose canonical encoding
 * differs from the original. Treats absent / undefined as equivalent and
 * uses `normalizeForDiff` for fields whose empty shape is wire-equivalent
 * to absent (so the UI cannot accidentally promote a default to an
 * override).
 *
 * Returned record is suitable for `configApi.patchAgentOverrides`.
 */
export function diffPatchableAgentFields(
  current: AgentSpec,
  original: AgentSpec,
): Record<string, unknown> {
  const patch: Record<string, unknown> = {};
  for (const key of PATCHABLE_AGENT_FIELDS) {
    const a = normalizeForDiff(key, current[key]);
    const b = normalizeForDiff(key, original[key]);
    if (!deepEqualCanonical(a, b)) {
      patch[key] = current[key];
    }
  }
  return patch;
}

/**
 * Build the clone payload for `cloneFromExisting`. Strips provenance fields
 * so the new record is treated as locally-defined and the user has to pick a
 * fresh `id` before Save.
 *
 * `endpoint` is also cleared. Endpoint binds the agent to a specific remote
 * backend and is not editable from this form — keeping it on a clone would
 * silently re-target the new agent at the source's remote backend with no
 * affordance to change it. Users who actually want a remote-backed clone
 * must (re)configure the endpoint after cloning via a code path that
 * exposes the form.
 */
export function cloneAgentSpecForEditor(source: AgentSpec): AgentSpec {
  return {
    ...source,
    id: "",
    created_at: undefined,
    updated_at: undefined,
    registry: undefined,
    endpoint: undefined,
  };
}

export interface PluginToggleResult {
  plugin_ids: string[];
  /**
   * `undefined` means "field absent" — kept absent when it was absent
   * before, so toggling never introduces an empty `active_hook_filter`
   * field that would show up in the patch diff.
   */
  active_hook_filter: string[] | undefined;
}

/**
 * Compute the next `plugin_ids` / `active_hook_filter` pair when the user
 * toggles a plugin on or off. When a plugin is removed, drops the same id
 * from `active_hook_filter` so re-enabling later doesn't silently pick up
 * a stale filter entry.
 */
export function togglePluginState(
  pluginIds: string[] | undefined,
  activeHookFilter: string[] | undefined,
  pluginId: string,
): PluginToggleResult {
  const current = pluginIds ?? [];
  const removing = current.includes(pluginId);
  const next = removing ? current.filter((id) => id !== pluginId) : [...current, pluginId];

  const nextFilter =
    removing && activeHookFilter
      ? activeHookFilter.filter((id) => id !== pluginId)
      : activeHookFilter;

  return { plugin_ids: next, active_hook_filter: nextFilter };
}

export interface HookFilterPartition {
  /** Filter entries that match a currently-loaded plugin. */
  active: string[];
  /**
   * Filter entries with no matching plugin in `plugin_ids`. These would not
   * gate any hook at runtime but stay in the saved record. The editor
   * surfaces them so the user can clear them deliberately rather than
   * silently dropping them.
   */
  stale: string[];
}

/**
 * Split an `active_hook_filter` value against the agent's current
 * `plugin_ids` so the UI can render valid entries as togglable rows and
 * stale entries as a separate warning group.
 */
export function partitionActiveHookFilter(
  filter: string[] | undefined,
  pluginIds: string[] | undefined,
): HookFilterPartition {
  const filterValues = filter ?? [];
  const pluginSet = new Set(pluginIds ?? []);
  const active: string[] = [];
  const stale: string[] = [];
  const seen = new Set<string>();
  for (const id of filterValues) {
    if (seen.has(id)) continue;
    seen.add(id);
    if (pluginSet.has(id)) {
      active.push(id);
    } else {
      stale.push(id);
    }
  }
  return { active, stale };
}

/** Keys that always carry secret material when present in a `RemoteEndpoint`
 *  auth object — masked when the editor displays an endpoint read-only.
 *  Lowercased contains-match: covers `token`, `bearer_token`,
 *  `access_token`, `refresh_token`, `id_token`, etc. */
const SENSITIVE_AUTH_KEY_PATTERNS = [
  "token",
  "secret",
  "password",
  "passphrase",
  "api_key",
  "apikey",
  "authorization",
  "credential",
  "private_key",
  "privatekey",
  "client_secret",
];

const REDACTED_PLACEHOLDER = "***";

function isSensitiveKey(key: string): boolean {
  const lowered = key.toLowerCase();
  return SENSITIVE_AUTH_KEY_PATTERNS.some((pattern) => lowered.includes(pattern));
}

function redactRecord(value: unknown): unknown {
  if (value === null || value === undefined) return value;
  if (Array.isArray(value)) return value.map(redactRecord);
  if (typeof value === "object") {
    const next: Record<string, unknown> = {};
    for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
      if (isSensitiveKey(key)) {
        next[key] = inner === null || inner === undefined ? inner : REDACTED_PLACEHOLDER;
      } else {
        next[key] = redactRecord(inner);
      }
    }
    return next;
  }
  return value;
}

/**
 * Return a copy of `endpoint` with secret-bearing keys (`bearer_token`,
 * `api_key`, `authorization`, etc.) replaced by `"***"`. The non-secret
 * shape (`backend`, `base_url`, `target`, `timeout_ms`, auth `type`, etc.)
 * is preserved so the read-only UI is still useful for verifying that an
 * agent is wired to the expected remote, without leaking the credential
 * into the admin DOM.
 *
 * Walks the entire endpoint (not just `auth`) defensively: any future
 * schema addition that happens to carry credentials would still be masked
 * by virtue of its key name.
 */
export function redactEndpointForDisplay(endpoint: RemoteEndpoint): RemoteEndpoint {
  return redactRecord(endpoint) as RemoteEndpoint;
}

/**
 * Return a copy of `spec` safe for display in the admin DOM: same shape,
 * but `endpoint` (the only field today that can carry remote-backend
 * credentials) is run through `redactEndpointForDisplay`. Used by the Raw
 * JSON editor and history-restore confirm dialog so a real bearer token
 * never lands in a textarea or a confirmation popover.
 */
export function redactAgentSpecForDisplay(spec: AgentSpec): AgentSpec {
  if (!spec.endpoint) return spec;
  return { ...spec, endpoint: redactEndpointForDisplay(spec.endpoint) };
}

/**
 * The complete set of `AgentSpec` keys the editor knows how to round-trip.
 * Combines identity (server-managed), locked (provenance / not editable),
 * and patchable (user-editable) fields. Used by the Raw JSON Apply path to
 * reject unknown top-level keys before they enter editor state and silently
 * disappear on Save (which only persists `PATCHABLE_AGENT_FIELDS` on the
 * customized PATCH path).
 */
export const ALLOWED_AGENT_FIELDS: ReadonlyArray<keyof AgentSpec> = [
  "id",
  "created_at",
  "updated_at",
  ...LOCKED_AGENT_FIELDS,
  ...PATCHABLE_AGENT_FIELDS,
];

/**
 * Returns the top-level keys of `parsed` that the editor cannot persist —
 * either future fields the UI hasn't learned about yet, or typos. The
 * caller should refuse the Apply rather than letting the field enter
 * editor state and disappear on Save.
 */
export function unknownAgentSpecFields(parsed: Record<string, unknown>): string[] {
  const allowed = new Set<string>(ALLOWED_AGENT_FIELDS);
  return Object.keys(parsed).filter((key) => !allowed.has(key));
}

/**
 * Overlay the current spec's locked fields (`endpoint`, `registry`) onto a
 * parsed Raw JSON payload. The Raw JSON textarea shows a redacted view of
 * locked fields, so users would either need to type real credentials back
 * in to round-trip or `lockedFieldChange` would reject the apply on the
 * `***` placeholder. By overlaying the real values before the
 * locked-field-change check, redaction stays purely a display concern.
 *
 * Returns a shallow copy; never mutates the inputs.
 */
export function mergeLockedFields(
  parsed: Record<string, unknown>,
  spec: AgentSpec,
): Record<string, unknown> {
  const merged: Record<string, unknown> = { ...parsed };
  for (const field of LOCKED_AGENT_FIELDS) {
    const current = spec[field];
    if (current === undefined) {
      delete merged[field];
    } else {
      merged[field] = current;
    }
  }
  return merged;
}
