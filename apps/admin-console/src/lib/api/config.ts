import { agentsApi } from "./agents";
import { auditApi } from "./audit";
import { capabilitiesApi } from "./capabilities";
import { configResourceApi } from "./config-resource";
import { mcpApi } from "./mcp";
import { providersApi } from "./providers";
import { systemApi } from "./system";
import { toolsApi } from "./tools";

export const configApi = {
  ...configResourceApi,
  ...capabilitiesApi,
  ...providersApi,
  ...mcpApi,
  ...systemApi,
  ...agentsApi,
  ...auditApi,
  ...toolsApi,
};
