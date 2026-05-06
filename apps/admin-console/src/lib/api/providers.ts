import { BACKEND_URL, fetchJson } from "./http";
import type { ProviderTestResponse } from "./types";

export const providersApi = {
  testProvider: (id: string) =>
    fetchJson<ProviderTestResponse>(`${BACKEND_URL}/v1/providers/${encodeURIComponent(id)}/test`, {
      method: "POST",
    }),
};
