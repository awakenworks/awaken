export interface MemoryStore {
  id: string;
  type: "memory_store";
  name: string;
  description?: string | null;
  metadata: Record<string, string>;
  archived_at?: string | null;
  created_at: string;
  updated_at: string;
}

export interface MemoryEntry {
  id: string;
  type: "memory";
  memory_store_id: string;
  memory_version_id: string;
  path: string;
  content?: string | null;
  content_sha256: string;
  content_size_bytes: number;
  created_at: string;
  updated_at: string;
}

export interface MemoryVersion {
  id: string;
  type: "memory_version";
  memory_id: string;
  memory_store_id: string;
  operation: "created" | "modified" | "deleted";
  path: string;
  content?: string | null;
  content_sha256?: string | null;
  content_size_bytes?: number | null;
  created_at: string;
  redacted_at?: string | null;
}
