import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";
import type { Capabilities, CapabilitiesResult } from "./types";

export const EMPTY_CAPABILITIES: Capabilities = {
  agents: [],
  tools: [],
  plugins: [],
  skills: [],
  models: [],
  providers: [],
  namespaces: [],
};

function normalizeCapabilities(capabilities: Capabilities): Capabilities {
  return {
    ...capabilities,
    skills: (capabilities.skills ?? []).map((skill) => {
      const allowedTools = skill.allowed_tools ?? [];
      const argumentsList = skill.arguments ?? [];
      const paths = skill.paths ?? [];
      return {
        ...skill,
        allowed_tools: allowedTools,
        arguments: argumentsList,
        paths,
      };
    }),
  };
}

export function capabilitiesFromResult(
  result?: CapabilitiesResult | Capabilities | null,
): Capabilities | null {
  if (!result) return null;
  if ("kind" in result) {
    return result.kind === "ok" ? result.capabilities : null;
  }
  return result;
}

export const capabilitiesApi = {
  capabilities: async (): Promise<CapabilitiesResult> => {
    try {
      return {
        kind: "ok",
        capabilities: normalizeCapabilities(
          await fetchJson<Capabilities>(`${BACKEND_URL}/v1/capabilities`),
        ),
      };
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 404) {
        return { kind: "route_absent" };
      }
      if (err instanceof ConfigApiError && err.status === 503) {
        return { kind: "registry_unavailable", message: err.message };
      }
      throw err;
    }
  },
};
