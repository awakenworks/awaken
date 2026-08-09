import type { SandboxConfig } from "./types";

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
export interface PolicyCap {
  id: string;
  config_section: string;
  config_schema: Record<string, unknown>;
}
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
export interface Capabilities {
  runtime_version: string;
  tools: ToolCap[];
  plugins: PluginCap[];
  policies?: PolicyCap[];
  runtimes?: RuntimeCap[];
  sandbox?: SandboxCapability;
}
