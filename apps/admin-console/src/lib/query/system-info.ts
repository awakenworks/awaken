import { ConfigApiError, systemApi, type SystemInfo } from "../api";

export async function loadOptionalSystemInfo(): Promise<SystemInfo | null> {
  try {
    return await systemApi.systemInfo();
  } catch (error) {
    if (error instanceof ConfigApiError && (error.status === 404 || error.status === 503)) {
      return null;
    }
    throw error;
  }
}
