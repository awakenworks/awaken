import {
  type AgentSpec,
  type ConfigSourceState,
  type RecordMeta,
  ConfigApiError,
  configApi,
} from "@/lib/config-api";

export const EMPTY_AGENT: AgentSpec = {
  id: "",
  model_id: "",
  system_prompt: "",
  max_rounds: 16,
  max_continuation_retries: 2,
  plugin_ids: [],
  sections: {},
  delegates: [],
};

export const PATCHABLE_FIELDS: Array<keyof AgentSpec> = [
  "model_id",
  "system_prompt",
  "max_rounds",
  "max_continuation_retries",
  "plugin_ids",
  "sections",
  "allowed_tools",
  "excluded_tools",
  "delegates",
  "reasoning_effort",
];

export type AgentSaveMode = "create" | "patch-overrides" | "put-full-spec";

export function agentSaveMode(
  isNew: boolean,
  sourceState: ConfigSourceState | null,
): AgentSaveMode {
  if (isNew) return "create";
  return sourceState === "builtin" || sourceState === "customized"
    ? "patch-overrides"
    : "put-full-spec";
}

export async function getOptionalAgentMeta(id: string): Promise<RecordMeta | null> {
  try {
    return await configApi.getMeta("agents", id);
  } catch (error) {
    if (error instanceof ConfigApiError && error.status === 404) {
      return null;
    }
    throw error;
  }
}

export function normalizeJson(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => normalizeJson(item));
  }

  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    const normalized: Record<string, unknown> = {};
    for (const key of Object.keys(record).sort()) {
      const next = normalizeJson(record[key]);
      if (next !== undefined) {
        normalized[key] = next;
      }
    }
    return normalized;
  }

  return value;
}

export function stableStringify(value: unknown): string {
  return JSON.stringify(normalizeJson(value));
}

export function prettyStableStringify(value: unknown): string {
  return JSON.stringify(normalizeJson(value), null, 2);
}

export function jsonSemanticallyEqual(left: unknown, right: unknown): boolean {
  return stableStringify(left) === stableStringify(right);
}

export function diffPatchableFields(
  current: AgentSpec,
  original: AgentSpec,
): Record<string, unknown> {
  const patch: Record<string, unknown> = {};
  for (const key of PATCHABLE_FIELDS) {
    const a = current[key];
    const b = original[key];
    if (!jsonSemanticallyEqual(a, b)) {
      patch[key] = a === undefined ? null : a;
    }
  }
  return patch;
}

export function fullAgentSavePayload(spec: AgentSpec): AgentSpec {
  return {
    ...spec,
    plugin_ids: [...(spec.plugin_ids ?? [])],
    delegates: [...(spec.delegates ?? [])],
  };
}

export function agentSavePayload(
  spec: AgentSpec,
  originalSpec: AgentSpec | null,
  mode: AgentSaveMode,
): AgentSpec | Record<string, unknown> {
  if (mode === "patch-overrides") {
    return diffPatchableFields(spec, originalSpec ?? spec);
  }
  return fullAgentSavePayload(spec);
}

export function hydrateAgentSpec(spec: AgentSpec): AgentSpec {
  return {
    sections: {},
    plugin_ids: [],
    delegates: [],
    ...spec,
  };
}
