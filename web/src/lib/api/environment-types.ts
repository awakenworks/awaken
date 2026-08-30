// Environment and Sandbox wire shapes. Kept together because Environment
// authoring selects the execution boundary later projected to a Sandbox policy.

export type EnvNetworking =
  | { type: "unrestricted" }
  | { type: "limited"; allowed_hosts?: string[]; allow_package_managers?: boolean; allow_mcp_servers?: boolean };

export interface EnvironmentPackages {
  type?: "packages";
  apt?: string[];
  cargo?: string[];
  gem?: string[];
  go?: string[];
  npm?: string[];
  pip?: string[];
}

/** A sandbox mount made visible inside the isolated worker. */
export interface SandboxMount {
  mount_path: string;
  access: "read_only" | "read_write";
}

/** Egress policy: full, deny-by-default allowlist, or none. */
export type SandboxNetwork =
  | { mode: "unrestricted" }
  | { mode: "allowlist"; hosts?: string[] }
  | { mode: "none" };

/** An Awaken sandbox-policy value projected to the backend `SandboxSpec`.
 * It is deliberately separate from the official Environment config union. */
export interface SandboxConfig {
  isolation?: "workdir" | "namespace" | "container";
  mounts?: SandboxMount[];
  network?: SandboxNetwork;
  limits?: { cpu_millis?: number | null; memory_bytes?: number | null };
}

export interface EnvironmentConfig {
  type: "cloud" | "self_hosted";
  networking?: EnvNetworking;
  packages?: EnvironmentPackages;
}

export interface Environment {
  id: string;
  type: "environment";
  name: string;
  description?: string;
  config: EnvironmentConfig;
  metadata: Record<string, string>;
  archived_at?: string | null;
  created_at: string;
  updated_at: string;
}

export type SandboxProvisioning = "eager" | "on_tool_use";

export interface SandboxExecutionPolicy {
  id: string;
  version: number;
  config: SandboxConfig;
  provisioning: SandboxProvisioning;
  disabled: boolean;
}

export interface SandboxPolicyBinding {
  environment_id: string;
  policy_id?: string;
  version?: number;
  provisioning: SandboxProvisioning;
}

/** An environment's durable work queue state (GET /v1/environments/:id/work/stats):
 * `depth` = items queued (waiting to be claimed), `pending` = items a worker has
 * claimed and is processing. */
export interface WorkQueueStats {
  type: "work_queue_stats";
  depth: number;
  pending: number;
  oldest_queued_at?: string | null;
  workers_polling: number;
}
