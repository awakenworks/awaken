import { type AgentSpec, type RecordMeta, ConfigApiError, configApi } from "@/lib/config-api";

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

export type AgentSaveMode = "create" | "patch-overrides" | "put-full-spec";

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

export function hydrateAgentSpec(spec: AgentSpec): AgentSpec {
  return {
    sections: {},
    plugin_ids: [],
    delegates: [],
    ...spec,
  };
}
