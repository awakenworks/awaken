// A lightweight JSON-Schema form renderer — enough to make plugin `config_schema`
// sections (permission-style rules, state machines, compaction knobs) editable as
// structured forms instead of raw JSON. Covers object / string / number / integer
// / boolean / enum / array; ANY node it can't model (oneOf/anyOf/$ref/untyped)
// falls back to a validated JSON textarea, so it never blocks authoring.

import { useState } from "react";
import { cx } from "./cx";

export interface JsonSchema {
  type?: string | string[];
  title?: string;
  description?: string;
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  enum?: unknown[];
  default?: unknown;
  [k: string]: unknown;
}

function firstType(s: JsonSchema): string | undefined {
  return Array.isArray(s.type) ? s.type[0] : s.type;
}

function defaultFor(s: JsonSchema): unknown {
  if (s.default !== undefined) return s.default;
  switch (firstType(s)) {
    case "object":
      return {};
    case "array":
      return [];
    case "boolean":
      return false;
    case "number":
    case "integer":
      return 0;
    case "string":
      return s.enum ? (s.enum[0] ?? "") : "";
    default:
      return null;
  }
}

/** Nodes we render structurally; everything else drops to a JSON textarea. */
function renderable(s: JsonSchema): boolean {
  if (s.oneOf || s.anyOf || s.allOf || s.$ref) return false;
  const t = firstType(s);
  return t === "object" || t === "string" || t === "number" || t === "integer" || t === "boolean" || t === "array";
}

function JsonFallback({ value, onChange }: { value: unknown; onChange: (v: unknown) => void }) {
  const [draft, setDraft] = useState<string | null>(null);
  const [err, setErr] = useState("");
  const text = draft ?? JSON.stringify(value ?? null, null, 2);
  return (
    <div>
      <textarea
        className="input mono"
        rows={4}
        value={text}
        onChange={(e) => {
          setDraft(e.target.value);
          try {
            onChange(e.target.value.trim() === "" ? null : JSON.parse(e.target.value));
            setErr("");
          } catch {
            setErr("invalid JSON — not saved");
          }
        }}
      />
      {err && <span className="err" style={{ fontSize: 11 }}>{err}</span>}
    </div>
  );
}

function Node({ schema, value, onChange }: { schema: JsonSchema; value: unknown; onChange: (v: unknown) => void }) {
  if (!renderable(schema)) return <JsonFallback value={value} onChange={onChange} />;
  const t = firstType(schema);

  if (schema.enum) {
    return (
      <select className="input" value={String(value ?? "")} onChange={(e) => onChange(e.target.value)}>
        {schema.enum.map((o) => (
          <option key={String(o)} value={String(o)}>
            {String(o)}
          </option>
        ))}
      </select>
    );
  }
  if (t === "boolean") {
    return (
      <label className="row" style={{ gap: 6 }}>
        <input type="checkbox" checked={!!value} onChange={(e) => onChange(e.target.checked)} style={{ accentColor: "var(--accent)" }} />
        <span className="mut">{schema.description ?? ""}</span>
      </label>
    );
  }
  if (t === "number" || t === "integer") {
    return (
      <input
        className="input mono"
        type="number"
        value={value === null || value === undefined ? "" : Number(value)}
        onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
      />
    );
  }
  if (t === "string") {
    return <input className="input" value={String(value ?? "")} onChange={(e) => onChange(e.target.value)} />;
  }
  if (t === "array") {
    const arr = Array.isArray(value) ? value : [];
    const items = schema.items ?? {};
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
        {arr.map((el, i) => (
          <div key={i} className="row" style={{ alignItems: "flex-start" }}>
            <div style={{ flex: 1 }}>
              <Node schema={items} value={el} onChange={(v) => onChange(arr.map((x, j) => (j === i ? v : x)))} />
            </div>
            <button className="btn ghost" style={{ height: 24 }} onClick={() => onChange(arr.filter((_, j) => j !== i))}>
              ✕
            </button>
          </div>
        ))}
        <button className="btn ghost" style={{ height: 24, alignSelf: "flex-start" }} onClick={() => onChange([...arr, defaultFor(items)])}>
          + add
        </button>
      </div>
    );
  }
  // object
  const obj = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  const props = schema.properties ?? {};
  const required = schema.required ?? [];
  return (
    <div className="schema-object">
      {Object.entries(props).map(([key, sub]) => (
        <div className="field" key={key}>
          <label style={{ textTransform: "none", letterSpacing: 0, fontSize: 12 }}>
            {sub.title ?? key}
            {required.includes(key) && <span style={{ color: "var(--danger)" }}> *</span>}
          </label>
          {sub.description && firstType(sub) !== "boolean" && <span className="mut">{sub.description}</span>}
          <Node schema={sub} value={obj[key]} onChange={(v) => onChange({ ...obj, [key]: v })} />
        </div>
      ))}
    </div>
  );
}

export interface SchemaFormProps {
  schema: JsonSchema;
  value: unknown;
  onChange: (value: unknown) => void;
  className?: string;
}

/** Render `value` as a form driven by `schema`. Unsupported subtrees fall back to
 * a JSON textarea. */
export function SchemaForm({ schema, value, onChange, className }: SchemaFormProps) {
  return (
    <div className={cx("schema-form", className)}>
      <Node schema={schema} value={value} onChange={onChange} />
    </div>
  );
}
