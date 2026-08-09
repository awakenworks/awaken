/** One FileCatalog projection. An Agent output is distinguished by purpose and
 * Session/path provenance; it does not become a second Artifact aggregate. */
export interface FileArtifact {
  id: string;
  type: string;
  filename: string;
  mime_type?: string;
  size_bytes?: number;
  created_at?: string;
  downloadable?: boolean;
  purpose?: "input" | "artifact";
  session_id?: string | null;
  logical_path?: string | null;
}

export interface FileListResponse {
  data: FileArtifact[];
  has_more: boolean;
  first_id?: string | null;
  last_id?: string | null;
}
