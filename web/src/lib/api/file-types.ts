/** One official beta Files projection. A Session-scoped File is an output;
 * an unscoped File is a reusable Workspace input. */
export interface FileScope {
  id: string;
  type: "session";
}

export interface FileArtifact {
  id: string;
  type: string;
  filename: string;
  mime_type?: string;
  size_bytes?: number;
  created_at?: string;
  downloadable?: boolean;
  scope?: FileScope | null;
}

export interface FileListResponse {
  data: FileArtifact[];
  has_more: boolean;
  first_id?: string | null;
  last_id?: string | null;
}
