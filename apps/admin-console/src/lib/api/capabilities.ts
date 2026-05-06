import { BACKEND_URL, fetchJson } from "./http";
import type { Capabilities } from "./types";

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

export const capabilitiesApi = {
  capabilities: async () =>
    normalizeCapabilities(await fetchJson<Capabilities>(`${BACKEND_URL}/v1/capabilities`)),
};
