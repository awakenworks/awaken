export interface Vault {
  id: string;
  type: "vault";
  display_name?: string;
  archived_at?: string | null;
  [key: string]: unknown;
}

/** Secret-free credential projection; credential kind and fields live under `auth`. */
export interface VaultCredentialAuth {
  type: "environment_variable" | "static_bearer" | "mcp_oauth";
  mcp_server_url?: string;
  secret_name?: string;
  expires_at?: string;
  [key: string]: unknown;
}

export interface VaultCredential {
  id: string;
  type: string;
  auth?: VaultCredentialAuth;
  display_name?: string;
  [key: string]: unknown;
}
