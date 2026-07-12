// A pure, framework-free diff over two config objects (the domain projection behind the
// publish-preview). It recurses into plain objects and reports leaf-level changes with a
// dotted path; arrays and scalars are compared whole. Kept out of React so it is trivially
// unit-tested and reusable by any config surface (agents today, models/mcp later).

export type ChangeKind = "added" | "removed" | "changed";

export interface Change {
  /** Dotted path to the changed leaf, e.g. `plugin_config.compact.threshold`. */
  path: string;
  kind: ChangeKind;
  before?: unknown;
  after?: unknown;
}

const isPlainObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

const equal = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);

/** The ordered leaf-level changes turning `before` into `after`. Empty when equal. */
export function diffConfig(before: unknown, after: unknown, base = ""): Change[] {
  if (equal(before, after)) return [];
  if (isPlainObject(before) && isPlainObject(after)) {
    const keys = Array.from(new Set([...Object.keys(before), ...Object.keys(after)])).sort();
    return keys.flatMap((k) => diffConfig(before[k], after[k], base ? `${base}.${k}` : k));
  }
  if (before === undefined) return [{ path: base, kind: "added", after }];
  if (after === undefined) return [{ path: base, kind: "removed", before }];
  return [{ path: base, kind: "changed", before, after }];
}

/** Friendly label for a config path — the ubiquitous term, not the field name. Falls back
 * to the top-level segment's label, then to the raw path (nothing is ever hidden). */
const LABELS: Record<string, string> = {
  name: "Name",
  description: "Description",
  system: "System instructions",
  model: "Model",
  max_steps: "Max steps",
  tools: "Tools",
  plugins: "Enabled behaviors",
  plugin_config: "Behavior config",
  context_policy: "Context policy",
  tool_overrides: "Tool presentation",
  mcp_servers: "MCP servers",
  skills: "Skills",
};

export function labelForPath(path: string): string {
  return LABELS[path] ?? LABELS[path.split(".")[0]] ?? path;
}

export type EditorSection = "overview" | "behavior" | "tools" | "resources";

/** Which editor section a config path lives under — so a validation issue (or a diff row)
 * routes the user to the right place. */
export function sectionForPath(path: string): EditorSection {
  const top = path.split(".")[0];
  if (top === "tools" || top === "tool_overrides") return "tools";
  if (top === "plugins" || top === "plugin_config" || top === "context_policy") return "behavior";
  return "overview"; // model / system / max_steps / name / description / whole-config
}
