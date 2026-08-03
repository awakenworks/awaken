// Managed Agents Dream research-preview and Awaken automation-policy wire types.

export type DreamStatus = "pending" | "running" | "completed" | "failed" | "canceled";

export interface DreamModelConfig {
  id: string;
  speed?: "standard" | "fast";
}

export type DreamInput =
  | { type: "memory_store"; memory_store_id: string }
  | { type: "sessions"; session_ids: string[] };

export interface DreamOutput {
  type: "memory_store";
  memory_store_id: string;
}

export interface DreamUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
}

export interface Dream {
  id: string;
  type: "dream";
  archived_at?: string | null;
  created_at: string;
  ended_at?: string | null;
  error?: { type: string; message: string } | null;
  inputs: DreamInput[];
  instructions?: string | null;
  model: DreamModelConfig;
  outputs: DreamOutput[];
  session_id?: string | null;
  status: DreamStatus;
  usage: DreamUsage;
}

export interface DreamPage {
  data: Dream[];
  next_page: string | null;
}

export interface DreamCapability {
  enabled: boolean;
  research_preview: boolean;
  supported_models: string[];
  supported_speeds: string[];
  max_sessions: number;
  max_instructions_chars: number;
  policy_available: boolean;
  collection_path: string;
  policy_path_template: string;
}

export interface ManagedModel {
  id: string;
  type: "model";
  display_name: string;
}

export interface ManagedModelPage {
  data: ManagedModel[];
  has_more: boolean;
  first_id?: string | null;
  last_id?: string | null;
}

export interface PlatformCapabilities {
  dreams?: DreamCapability;
  [key: string]: unknown;
}

export interface DreamPolicyConfig {
  enabled: boolean;
  interval_seconds: number;
  min_new_sessions: number;
  max_sessions: number;
  model: DreamModelConfig;
  instructions?: string | null;
}

export interface DreamPolicy extends DreamPolicyConfig {
  type: "dream_policy";
  memory_store_id: string;
  next_due_at?: string | null;
  last_completed_cutoff_at?: string | null;
}
