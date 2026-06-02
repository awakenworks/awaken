import { BACKEND_URL, fetchJson } from "./http";
import type { A2aServerStatusResponse } from "./types";

export const a2aApi = {
  a2aStatus: (id: string) =>
    fetchJson<A2aServerStatusResponse>(
      `${BACKEND_URL}/v1/a2a-servers/${encodeURIComponent(id)}/status`,
    ),
};
