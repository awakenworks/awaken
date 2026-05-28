/// Pure helpers used by `AgentEditorPage`. They are factored out here so they
/// stay unit-testable without having to drive React Router + the page state
/// machine, and so the JSON-editor / plugin-toggle / clone / patch-diff /
/// remote-endpoint-display code paths share one canonical implementation.
import type { AgentSpec } from "./api/types";
import { deepEqualCanonical } from "./agent-editor-canonical";

export { canonicalStringify, deepEqualCanonical } from "./agent-editor-canonical";
export { safeErrorMessage } from "./safe-error-message";
export {
  changedRedactionMarkerPaths,
  computeRedactedDiff,
  redactAgentSpecForDisplay,
  redactAgentSpecForEditing,
  redactEndpointForDisplay,
  redactSecretString,
  redactSecretsForDisplay,
  restoreUnchangedRedactions,
} from "./agent-secret-redaction";
export type {
  RedactedAgentSpecForEditing,
  RedactedFieldChange,
  RedactionEntry,
  RedactionPathSegment,
} from "./agent-secret-redaction";

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
export const PATCHABLE_AGENT_FIELDS = [
  "model_id",
  "system_prompt",
  "max_rounds",
  "max_continuation_retries",
  "context_policy",
  "plugin_ids",
  "active_hook_filter",
  "sections",
  "allowed_tools",
  "allowed_tool_patterns",
  "excluded_tools",
  "excluded_tool_patterns",
  "delegates",
  "reasoning_effort",
] as const satisfies ReadonlyArray<keyof AgentSpec>;

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
 * Save plan for a builtin / customized record. Both lists ride a single
 * `PATCH /v1/config/agents/:id/overrides` body shaped like
 * `{...patch, _clear: [...clear]}`; the server applies upserts and
 * clears in one `apply_locked` transaction (R11 #3, supersedes the
 * earlier PATCH + N×DELETE flow).
 *
 *  - `patch`  → top-level body keys. Includes explicit `null` for
 *               nullable fields the user deliberately disabled (e.g.
 *               `context_policy: null` = "no policy even if base had one").
 *  - `clear`  → entries in the `_clear` array. Removes the field from
 *               `user_overrides` so the resolved spec falls back to
 *               the builtin default — distinct from "explicit null
 *               override" which would keep the customized badge on.
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

export interface PluginConfigSchemaDefaults {
  key: string;
  default_value?: unknown;
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
  } else if (!removing && activeHookFilter && activeHookFilter.length > 0) {
    nextFilter = activeHookFilter.includes(pluginId)
      ? [...activeHookFilter]
      : [...activeHookFilter, pluginId];
  } else {
    nextFilter = activeHookFilter;
  }

  return { plugin_ids: next, active_hook_filter: nextFilter };
}

export function applyPluginSectionDefaults(
  sections: Record<string, unknown> | undefined,
  schemas: PluginConfigSchemaDefaults[],
): Record<string, unknown> {
  const next = { ...(sections ?? {}) };

  for (const schema of schemas) {
    if (next[schema.key] !== undefined) {
      continue;
    }
    next[schema.key] = cloneJsonValue(schema.default_value ?? {});
  }

  return next;
}

function cloneJsonValue(value: unknown): unknown {
  if (value === undefined) {
    return {};
  }
  return JSON.parse(JSON.stringify(value));
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

/**
 * The complete set of `AgentSpec` keys the editor knows how to round-trip.
 * Combines identity (server-managed), locked (provenance / not editable),
 * and patchable (user-editable) fields. Used by the Raw JSON Apply path to
 * reject unknown top-level keys before they enter editor state and silently
 * disappear on Save (which only persists `PATCHABLE_AGENT_FIELDS` on the
 * customized PATCH path).
 */
export const ALLOWED_AGENT_FIELDS = [
  "id",
  "created_at",
  "updated_at",
  ...LOCKED_AGENT_FIELDS,
  ...PATCHABLE_AGENT_FIELDS,
] as const;

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
 * candidate carries the live values rather than the editing redaction sentinel.
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
