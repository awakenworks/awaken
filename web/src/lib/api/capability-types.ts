import type {
  AgentToolPermissionPolicy,
  InputResourceKind,
  SandboxConfig,
} from "./types";

export interface ToolCap {
  id: string;
  description: string;
  parameters: Record<string, unknown>;
}

export interface PluginCap {
  id: string;
  config_sections: string[];
  config_schema: Record<string, unknown>;
}

export interface AgentToolsetMemberCap {
  name: string;
  description?: string | null;
  input_schema?: Record<string, unknown> | null;
  available: boolean;
  controlled_modification: boolean;
  configurable_fields: string[];
}

export interface ManagedToolsetDefaultCap {
  enabled: boolean;
  permission_policy: AgentToolPermissionPolicy;
}

export interface AgentToolsetCap {
  type: "agent_toolset_20260401";
  source_kind: "agent";
  dynamic_members: false;
  default_config: ManagedToolsetDefaultCap;
  members: AgentToolsetMemberCap[];
}

export interface McpToolsetCap {
  type: "mcp_toolset";
  source_kind: "mcp";
  dynamic_members: true;
  default_config: ManagedToolsetDefaultCap;
  member_configurable_fields: string[];
}

/** One capability union; the `type` discriminant owns source-specific fields. */
export type ManagedToolsetCap = AgentToolsetCap | McpToolsetCap;

export interface RuntimeCap {
  id: string;
  label: string;
  kind: "native" | "acp";
  cli?: string | null;
  description: string;
  local?: {
    detected: boolean;
    version?: string | null;
    login_state?: string | null;
    reason_code?: string | null;
    remediation?: string | null;
    negotiated?: {
      modes: Array<{ native_id: string; name: string; description?: string | null; current: boolean }>;
      config_options: Array<{
        native_id: string;
        name: string;
        description?: string | null;
        current_value: string;
        choices: Array<{ native_value: string; name: string; description?: string | null }>;
      }>;
    } | null;
  } | null;
}

export interface SandboxPreset {
  id: string;
  label: string;
  description: string;
  spec: SandboxConfig;
}

export interface SandboxCapability {
  config_schema: Record<string, unknown>;
  presets: SandboxPreset[];
}

export type ResourceInputDefaultMounts = Record<InputResourceKind, string>;

export interface ResourceInputCapability {
  default_mounts: ResourceInputDefaultMounts;
}

export interface Capabilities {
  runtime_version: string;
  tools: ToolCap[];
  toolsets?: ManagedToolsetCap[];
  plugins: PluginCap[];
  runtimes?: RuntimeCap[];
  resource_inputs?: ResourceInputCapability;
  sandbox?: SandboxCapability;
}
