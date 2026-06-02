import { agentsApi } from "./agents";
import { a2aApi } from "./a2a";
import { auditApi } from "./audit";
import { capabilitiesApi } from "./capabilities";
import { configResourceApi } from "./config-resource";
import { mcpApi } from "./mcp";
import { providersApi } from "./providers";
import { systemApi } from "./system";
import { toolsApi } from "./tools";

export const configApi = {
  ...configResourceApi,
  ...a2aApi,
  ...capabilitiesApi,
  ...providersApi,
  ...mcpApi,
  ...systemApi,
  ...agentsApi,
  ...auditApi,
  ...toolsApi,
};
