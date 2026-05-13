import {
  type AgentSpec,
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

export function diffPatchableFields(
  current: AgentSpec,
  original: AgentSpec,
): Record<string, unknown> {
  const patch: Record<string, unknown> = {};
  for (const key of PATCHABLE_FIELDS) {
    const a = current[key];
    const b = original[key];
    if (JSON.stringify(a) !== JSON.stringify(b)) {
      patch[key] = a;
    }
  }
  return patch;
}

export function hydrateAgentSpec(spec: AgentSpec): AgentSpec {
  return {
    sections: {},
    plugin_ids: [],
    delegates: [],
    ...spec,
  };
}
