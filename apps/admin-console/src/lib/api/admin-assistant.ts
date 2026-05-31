import { BACKEND_URL, fetchJson } from "./http";
import type { AdminAssistantConfig } from "./types";

const ADMIN_ASSISTANT_CONFIG_URL = `${BACKEND_URL}/v1/admin/assistant/config`;

export const adminAssistantApi = {
  getConfig: () => fetchJson<AdminAssistantConfig>(ADMIN_ASSISTANT_CONFIG_URL),
  updateConfig: (config: AdminAssistantConfig) =>
    fetchJson<AdminAssistantConfig>(ADMIN_ASSISTANT_CONFIG_URL, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(config),
    }),
};
