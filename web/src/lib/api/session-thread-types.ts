export interface SessionThreadAgent {
  id: string;
  type: "agent";
  version: number;
  model: string | { id: string };
  name: string;
  description?: string | null;
  tools: unknown[];
  mcp_servers: unknown[];
  skills: unknown[];
}

export interface SessionThreadUsage {
  cache_read_input_tokens?: number;
  input_tokens?: number;
  output_tokens?: number;
  cache_creation?: {
    ephemeral_1h_input_tokens?: number;
    ephemeral_5m_input_tokens?: number;
  };
}

export interface SessionThread {
  id: string;
  type: "session_thread";
  session_id: string;
  parent_thread_id: string | null;
  agent: SessionThreadAgent;
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
  status: "running" | "idle" | "rescheduling" | "terminated";
  stats?: {
    active_seconds?: number;
    duration_seconds?: number;
    startup_seconds?: number;
  } | null;
  usage?: SessionThreadUsage | null;
}
