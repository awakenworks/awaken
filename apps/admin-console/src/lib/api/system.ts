import { BACKEND_URL, fetchJson } from "./http";
import type { SystemInfo } from "./types";

export const systemApi = {
  /** Server identity + uptime + which optional subsystems are wired. */
  systemInfo: () => fetchJson<SystemInfo>(`${BACKEND_URL}/v1/system/info`),
};
