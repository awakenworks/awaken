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
 * fields are unchanged.
 *
 * **Normalization contract**: this guard exists to stop the Raw JSON
 * path from persisting locked-field edits the save flow can't carry. It
 * uses canonical deep equality, which collapses three writings the
 * runtime treats identically into one equivalence class:
 *
 *  - **key absent**       (`{}`)
 *  - **key present, null** (`{"endpoint": null}`)
 *  - **key present, undefined** (`{"endpoint": undefined}`)
 *
 * For locked fields whose current spec value is "no value" (absent / null),
 * an Apply that writes `null` or removes the key is intentionally a no-op
 * — not a "silent drop". The runtime, the customized PATCH layer (which
 * does NOT include endpoint/registry in `PATCHABLE_AGENT_FIELDS`), and
 * this guard all agree that those three forms mean the same thing.
 * Without this normalization, a user re-typing `endpoint: null` to
 * "make the absence explicit" would get rejected with a misleading
 * "locked field changed" error.
 *
 * For locked fields with a real value (e.g. `endpoint.base_url = "..."`),
 * any byte-level change to that value DOES surface here — the equivalence
 * class above only collapses the empty cases.
 *
 * Also key-order-insensitive: re-indenting / reordering object keys in
 * Raw JSON doesn't falsely flag a locked-field change.
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
 * Plan of changes that need to ride the wire to persist the user's draft
 * against an existing builtin / customized agent record. Split into two
 * lists because they target different endpoints:
 *
 *  - `patch`  → `PATCH /v1/config/agents/:id/overrides` with this map as the
 *               body. Contains fields with concrete values, including
 *               explicit `null` for nullable fields the user deliberately
 *               disabled (e.g. `context_policy: null` meaning "no context
 *               policy on this agent, even if the base had one").
 *  - `clear`  → one `DELETE /v1/config/agents/:id/overrides/:field` per
 *               entry. These are fields the user reverted to "use the base
 *               value" — the `user_overrides` key gets removed entirely
 *               rather than left as an explicit-null override that would
 *               keep the "customized" badge on for no reason.
 *
 * Distinguishing the two is the difference between "explicit null
 * override" and "no override". A naive `PATCH {field: null}` for every
 * cleared field would mix the two: nullable fields would receive an
 * `explicit-null` override (badge stays customized) instead of being
 * cleared back to the builtin default.
 */
export interface AgentPatchPlan {
  patch: Record<string, unknown>;
  clear: Array<keyof AgentSpec>;
}

/**
 * Compute the changes a Save should apply to a builtin / customized
 * record. Walks `PATCHABLE_AGENT_FIELDS` once and emits each field to
 * exactly one of `patch` / `clear` / no-op.
 *
 * Two ordering subtleties:
 *
 *  1. `active_hook_filter` is special-cased. The runtime engine treats
 *     `[]` and absent identically on the resolved spec (both mean "all
 *     hooks run"), but the customized PATCH layer treats them
 *     differently: an absent override INHERITS the base's filter while
 *     `Some([])` EXPLICITLY overrides it. A user who clicks "All
 *     plugins" on a customized record whose base has a non-empty filter
 *     wants the override to turn filtering off — emitting CLEAR there
 *     would inherit the base filter and silently undo the choice. So
 *     when `current` is empty (`undefined` or `[]`) and `original` is
 *     non-empty, route to `patch: []` instead of `clear`. The
 *     "inherit base" semantic isn't user-reachable through the form
 *     today, so it's intentionally not represented here.
 *
 *  2. Clear-on-undefined is checked BEFORE `deepEqualCanonical`. The
 *     canonical encoding collapses `null` and `undefined` (both → JSON
 *     `null`), so a saved explicit-null override (e.g. `context_policy:
 *     null` meaning "no policy even if base has one") edited away in
 *     Raw JSON would read as "equal" under canonical comparison and the
 *     override would stay stuck as explicit-null. Detecting clear
 *     first routes `null → undefined` to the CLEAR list instead.
 */
export function diffPatchableAgentFields(current: AgentSpec, original: AgentSpec): AgentPatchPlan {
  const patch: Record<string, unknown> = {};
  const clear: Array<keyof AgentSpec> = [];
  for (const key of PATCHABLE_AGENT_FIELDS) {
    if (key === "sections") {
      const sectionsPatch = diffSectionsForPatch(current.sections, original.sections);
      if (sectionsPatch) {
        patch.sections = sectionsPatch;
      }
      continue;
    }
    if (key === "active_hook_filter") {
      const curArr = current.active_hook_filter ?? [];
      const origArr = original.active_hook_filter ?? [];
      if (deepEqualCanonical(curArr, origArr)) continue;
      // `[]` is the "override base to All plugins" signal — see the
      // doc comment above. Always route to `patch`, never to `clear`.
      patch.active_hook_filter = curArr;
      continue;
    }
    const a = current[key];
    const b = original[key];
    if (a === undefined && b !== undefined) {
      clear.push(key);
      continue;
    }
    if (deepEqualCanonical(a, b)) continue;
    patch[key] = a;
  }
  return { patch, clear };
}

function asSectionRecord(value: AgentSpec["sections"]): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  return Object.fromEntries(Object.entries(value).filter(([, item]) => item !== undefined));
}

function diffSectionsForPatch(
  current: AgentSpec["sections"],
  original: AgentSpec["sections"],
): Record<string, unknown> | null {
  const currentSections = asSectionRecord(current);
  const originalSections = asSectionRecord(original);
  const patch: Record<string, unknown> = {};
  const keys = new Set([...Object.keys(originalSections), ...Object.keys(currentSections)]);
  for (const sectionKey of keys) {
    const currentHas = Object.prototype.hasOwnProperty.call(currentSections, sectionKey);
    const originalHas = Object.prototype.hasOwnProperty.call(originalSections, sectionKey);
    if (!currentHas && originalHas) {
      patch[sectionKey] = null;
      continue;
    }
    if (!currentHas) continue;
    const nextValue = currentSections[sectionKey];
    if (originalHas && deepEqualCanonical(nextValue, originalSections[sectionKey])) continue;
    patch[sectionKey] = nextValue;
  }
  return Object.keys(patch).length > 0 ? patch : null;
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
 *
 * If the prune empties the filter, collapse `[]` back to `undefined` —
 * the runtime treats both identically (`[]` = "all hooks run", same as
 * absent) but the patch-diff path sees `[] != undefined` and would
 * emit a no-op override. R10 #6.
 */
export function togglePluginState(
  pluginIds: string[] | undefined,
  activeHookFilter: string[] | undefined,
  pluginId: string,
): PluginToggleResult {
  const current = pluginIds ?? [];
  const removing = current.includes(pluginId);
  const next = removing ? current.filter((id) => id !== pluginId) : [...current, pluginId];

  let nextFilter: string[] | undefined;
  if (removing && activeHookFilter) {
    const pruned = activeHookFilter.filter((id) => id !== pluginId);
    // Distinguish two cases:
    //   - filter was non-empty and now empty: collapse to undefined so
    //     dirty tracking and PATCH diffing don't treat `[]` (semantic
    //     "all hooks") as different from absent (also "all hooks").
    //   - filter was `[]` already: keep `[]` so we don't silently
    //     change an explicit-empty draft into absent.
    nextFilter = pruned.length === 0 && activeHookFilter.length > 0 ? undefined : pruned;
  } else {
    nextFilter = activeHookFilter;
  }

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

/** Keys that always carry secret material when seen anywhere in a
 *  payload — masked by `redactSecretsForDisplay` regardless of context
 *  (audit log diff, trace event payload, DiffModal). Keys are normalized
 *  before matching so `api_key`, `api-key`, and `x-api-key` all hit the
 *  same `apikey` pattern. */
const SENSITIVE_AUTH_KEY_PATTERNS = [
  "token",
  "secret",
  "password",
  "passphrase",
  "apikey",
  "authorization",
  "credential",
  "privatekey",
  "clientsecret",
  // R10 #4 — broaden generic-redaction coverage to HTTP-flavored
  // secret carriers that an `endpoint.auth` payload or a trace event
  // can plausibly hold under arbitrary key names.
  "cookie",
  "jwt",
  "bearer",
  "session",
  "accesskey",
];

const REDACTED_PLACEHOLDER = "***";

const HEADER_CONTAINER_KEYS = new Set(["headers", "requestheaders", "responseheaders"]);

const SENSITIVE_HEADER_KEY_PATTERNS = [
  "authorization",
  "proxyauthorization",
  "cookie",
  "setcookie",
  "apikey",
  "xapikey",
  "xauthtoken",
  "token",
];

export type RedactionPathSegment = string | number;

export interface RedactionEntry {
  path: RedactionPathSegment[];
  original: unknown;
  redacted: unknown;
}

function normalizeSecretKey(key: string): string {
  return key.toLowerCase().replace(/[-_\s]/g, "");
}

function isSensitiveKey(key: string): boolean {
  const normalized = normalizeSecretKey(key);
  return SENSITIVE_AUTH_KEY_PATTERNS.some((pattern) => normalized.includes(pattern));
}

function isSensitiveHeaderKey(parentKey: string, key: string): boolean {
  if (!HEADER_CONTAINER_KEYS.has(normalizeSecretKey(parentKey))) return false;
  const normalized = normalizeSecretKey(key);
  return SENSITIVE_HEADER_KEY_PATTERNS.some((pattern) => normalized.includes(pattern));
}

function cloneJsonValue<T>(value: T): T {
  if (value === undefined || value === null) return value;
  const encoded = JSON.stringify(value);
  if (encoded === undefined) return value;
  return JSON.parse(encoded) as T;
}

function recordRedaction(
  redactions: RedactionEntry[] | undefined,
  path: RedactionPathSegment[],
  original: unknown,
  redacted: unknown,
) {
  if (!redactions || deepEqualCanonical(original, redacted)) return;
  redactions.push({
    path: [...path],
    original: cloneJsonValue(original),
    redacted: cloneJsonValue(redacted),
  });
}

function redactRecord(
  value: unknown,
  parentKey: string = "",
  path: RedactionPathSegment[] = [],
  redactions?: RedactionEntry[],
): unknown {
  if (value === null || value === undefined) return value;
  // Primitive strings — Trace events / audit snapshots / DiffModal feed
  // arbitrary payloads through this redactor. A `payload.output` /
  // `body.message` field whose key name doesn't match the credential
  // pattern list can still carry `Authorization: …` headers, inline
  // `Bearer …` tokens, JWTs, `sk-…` keys, etc. Apply pattern-based
  // string redaction so every display path that calls
  // `redactSecretsForDisplay` shares one defensive layer.
  if (typeof value === "string") {
    const redacted = redactSecretString(value);
    recordRedaction(redactions, path, value, redacted);
    return redacted;
  }
  if (Array.isArray(value)) {
    return value.map((item, index) => redactRecord(item, "", [...path, index], redactions));
  }
  if (typeof value === "object") {
    // Default-deny on any nested `auth` object: `RemoteAuth` allows
    // arbitrary keys, so a credential under `jwt` / `cookie` / `session` /
    // `x-api-key` / `header` / `bearer` would slip past the pattern list
    // below. When we recurse into a value whose key was `auth`, mask
    // every entry except the human-readable `type` discriminator —
    // mirrors `redactEndpointForDisplay` but applies wherever `auth`
    // shows up in audit / trace / diff payloads.
    if (normalizeSecretKey(parentKey) === "auth") {
      const safeAuth: Record<string, unknown> = {};
      for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
        if (key === "type" || inner === null || inner === undefined) {
          safeAuth[key] = inner;
        } else {
          safeAuth[key] = REDACTED_PLACEHOLDER;
          recordRedaction(redactions, [...path, key], inner, REDACTED_PLACEHOLDER);
        }
      }
      return safeAuth;
    }
    const next: Record<string, unknown> = {};
    for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
      const nextPath = [...path, key];
      if (isSensitiveKey(key) || isSensitiveHeaderKey(parentKey, key)) {
        next[key] = inner === null || inner === undefined ? inner : REDACTED_PLACEHOLDER;
        recordRedaction(redactions, nextPath, inner, next[key]);
      } else {
        next[key] = redactRecord(inner, key, nextPath, redactions);
      }
    }
    return next;
  }
  return value;
}

/**
 * Return a copy of `endpoint` with secret-bearing fields masked to `"***"`.
 * Two layers of defense:
 *
 *   1. Pattern-based: walks the whole endpoint and masks any key whose name
 *      matches a known credential pattern (`token`, `secret`, `password`,
 *      `api_key`, `authorization`, `credential`, `private_key`, …). This
 *      catches secrets that happen to live anywhere in the endpoint tree.
 *   2. Default-deny on `endpoint.auth`: `RemoteAuth` has an index signature,
 *      so a future schema addition could carry a credential under a key
 *      name the pattern list doesn't know about (`cookie`, `jwt`, `header`,
 *      etc.). For the `auth` object specifically, mask every key except
 *      the human-readable `type` discriminator.
 *
 * Either layer alone would catch the documented secret keys; the
 * combination is intentional for forward-compatibility with schema drift.
 * The non-secret shape (`backend`, `base_url`, `target`, `timeout_ms`,
 * `type`, etc.) is preserved so the read-only UI is still useful for
 * verifying an agent is wired to the expected remote.
 */
export function redactEndpointForDisplay(endpoint: RemoteEndpoint): RemoteEndpoint {
  const generic = redactRecord(endpoint) as RemoteEndpoint;
  if (!generic.auth || typeof generic.auth !== "object") {
    return generic;
  }
  const auth = generic.auth as Record<string, unknown>;
  const safeAuth: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(auth)) {
    if (key === "type") {
      // The discriminator is needed for the reader to understand the
      // shape — it's not a credential.
      safeAuth[key] = value;
    } else if (value === null || value === undefined) {
      // Preserve null / undefined as semantic markers — they signal
      // "no value", not "redacted".
      safeAuth[key] = value;
    } else {
      safeAuth[key] = REDACTED_PLACEHOLDER;
    }
  }
  return { ...generic, auth: safeAuth as RemoteEndpoint["auth"] };
}

/**
 * Recursively mask every secret-keyed field in an arbitrary value tree.
 * Used by display paths that serialize structures we don't statically
 * know the shape of — audit-log `before`/`after` snapshots (may carry a
 * full `AgentSpec` including `endpoint.auth`), persisted trace events
 * (payloads may include serialized agent specs), and history-restore
 * confirmation dialogs.
 */
export function redactSecretsForDisplay<T>(value: T): T {
  return redactRecord(value) as T;
}

export interface RedactedAgentSpecForEditing {
  redacted: AgentSpec;
  redactions: RedactionEntry[];
}

export function redactAgentSpecForEditing(spec: AgentSpec): RedactedAgentSpecForEditing {
  const redactions: RedactionEntry[] = [];
  return {
    redacted: redactRecord(spec, "", [], redactions) as AgentSpec,
    redactions,
  };
}

function readPath(root: unknown, path: readonly RedactionPathSegment[]) {
  let current = root;
  for (const segment of path) {
    if (current === null || typeof current !== "object") {
      return { found: false, value: undefined };
    }
    if (!Object.prototype.hasOwnProperty.call(current, segment)) {
      return { found: false, value: undefined };
    }
    current = Array.isArray(current)
      ? current[Number(segment)]
      : (current as Record<string, unknown>)[String(segment)];
  }
  return { found: true, value: current };
}

function writePath(root: unknown, path: readonly RedactionPathSegment[], value: unknown): boolean {
  if (path.length === 0) return false;
  let current = root;
  for (const segment of path.slice(0, -1)) {
    if (current === null || typeof current !== "object") return false;
    current = Array.isArray(current)
      ? current[Number(segment)]
      : (current as Record<string, unknown>)[String(segment)];
  }
  if (current === null || typeof current !== "object") return false;
  const leaf = path[path.length - 1];
  if (Array.isArray(current)) {
    current[Number(leaf)] = cloneJsonValue(value);
  } else {
    (current as Record<string, unknown>)[String(leaf)] = cloneJsonValue(value);
  }
  return true;
}

export function restoreUnchangedRedactions<T>(parsed: T, redactions: readonly RedactionEntry[]): T {
  if (redactions.length === 0) return parsed;
  const restored = cloneJsonValue(parsed);
  for (const redaction of redactions) {
    const current = readPath(restored, redaction.path);
    if (current.found && deepEqualCanonical(current.value, redaction.redacted)) {
      writePath(restored, redaction.path, redaction.original);
    }
  }
  return restored;
}

export interface RedactedFieldChange {
  path: string;
  before: unknown;
  after: unknown;
  redactedValueChanged: boolean;
}

/**
 * Compute semantic changes from raw values, but return only redacted
 * before/after payloads for rendering. This preserves secret-only changes
 * in DiffModal without handing the DOM the original credential values.
 */
export function computeRedactedDiff(
  prev: Record<string, unknown>,
  curr: Record<string, unknown>,
  base = "",
): RedactedFieldChange[] {
  return computeRedactedDiffFrom(
    prev,
    curr,
    redactSecretsForDisplay(prev),
    redactSecretsForDisplay(curr),
    base,
  );
}

function computeRedactedDiffFrom(
  prev: Record<string, unknown>,
  curr: Record<string, unknown>,
  redactedPrev: Record<string, unknown>,
  redactedCurr: Record<string, unknown>,
  base: string,
): RedactedFieldChange[] {
  const out: RedactedFieldChange[] = [];
  const keys = new Set([...Object.keys(prev ?? {}), ...Object.keys(curr ?? {})]);
  for (const key of keys) {
    const path = base ? `${base}.${key}` : key;
    const beforeRaw = prev?.[key];
    const afterRaw = curr?.[key];
    const beforeDisplay = redactedPrev?.[key];
    const afterDisplay = redactedCurr?.[key];
    if (deepEqualCanonical(beforeRaw, afterRaw)) continue;
    if (
      isDiffRecord(beforeRaw) &&
      isDiffRecord(afterRaw) &&
      isDiffRecord(beforeDisplay) &&
      isDiffRecord(afterDisplay)
    ) {
      out.push(...computeRedactedDiffFrom(beforeRaw, afterRaw, beforeDisplay, afterDisplay, path));
    } else {
      out.push({
        path,
        before: beforeDisplay,
        after: afterDisplay,
        redactedValueChanged: deepEqualCanonical(beforeDisplay, afterDisplay),
      });
    }
  }
  return out;
}

function isDiffRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Mask common credential patterns embedded in arbitrary text. Used by
 * display paths that render raw string payloads — tool outputs, tool
 * error messages — where the key-based redactor doesn't apply because
 * there's no key/value structure to walk (R12 #3).
 *
 * Patterns covered (case-insensitive where reasonable):
 *  - `Authorization: <value>` / `Cookie: <value>` / `Set-Cookie: <value>` (full line)
 *  - `Bearer <token>` (inline anywhere in the string)
 *  - `<api_key|access_key|access_token|client_secret|refresh_token|id_token|bearer_token|password|secret|token|jwt>=<value>`
 *    (or with `:` separator) — masks the value
 *  - JWT-shaped tokens: `eyJ<base64>.<base64>.<base64>`
 *  - OpenAI-style `sk-…`, Stripe-style `sk_(live|test)_…`
 *
 * Intentionally conservative — false positives mask non-secret strings,
 * which is the right trade-off for an admin-console display layer.
 */
export function redactSecretString(input: string): string {
  if (typeof input !== "string" || input.length === 0) return input;
  let result = input;
  // Full-line header values.
  result = result.replace(/Authorization\s*:\s*[^\r\n]+/gi, "Authorization: ***");
  result = result.replace(/Set-Cookie\s*:\s*[^\r\n]+/gi, "Set-Cookie: ***");
  result = result.replace(/(^|\s|;)Cookie\s*:\s*[^\r\n]+/gi, "$1Cookie: ***");
  // Inline Bearer token.
  result = result.replace(/Bearer\s+[A-Za-z0-9._\-+/=]{8,}/gi, "Bearer ***");
  // key=value / key: value for known credential field names. Negative
  // lookahead prevents re-masking already-redacted output.
  result = result.replace(
    /\b(api[_-]?key|access[_-]?key|access[_-]?token|client[_-]?secret|refresh[_-]?token|id[_-]?token|bearer[_-]?token|password|secret|token|jwt)\s*[:=]\s*(?!\*\*\*)[^\s,;"'&}]+/gi,
    "$1=***",
  );
  // JWT — three dot-separated base64url segments starting with eyJ.
  result = result.replace(/\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g, "***");
  // OpenAI-style sk-<long>, Stripe-style sk_(live|test)_<long>.
  result = result.replace(/\bsk-[A-Za-z0-9_-]{16,}/g, "***");
  result = result.replace(/\bsk_(?:live|test)_[A-Za-z0-9]+/g, "***");
  return result;
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
 * parsed Raw JSON payload AFTER the caller has compared the parsed draft
 * against the redacted display copy. This order is load-bearing: merging
 * BEFORE the compare would overwrite `parsed.endpoint` with `spec.endpoint`
 * and the subsequent `lockedFieldChange` would always read "equal",
 * silently dropping any user edit to `base_url` / `auth.*` / `target` /
 * `registry`. The compare must run first against the redacted display
 * spec; this merge then re-introduces the real credentials so the
 * candidate carries the live values rather than the `***` redaction.
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
