// Renders the leaf-level changes between two config objects (the publish-preview). The
// diff is computed by the pure `diffConfig`; this only presents it, translating each path
// to its domain label (raw path shown alongside as a hint, so nothing is hidden).

import { diffConfig, labelForPath, type ChangeKind } from "../../lib/config-diff";
import { Pill } from "../ui";
import { useApp } from "../../lib/app-state";

const short = (v: unknown): string => {
  if (v === undefined) return "∅";
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > 90 ? `${s.slice(0, 90)}…` : s;
};

const TONE: Record<ChangeKind, "ok" | "warn" | "danger"> = {
  added: "ok",
  removed: "danger",
  changed: "warn",
};

export default function ConfigDiff({ before, after }: { before: unknown; after: unknown }) {
  const app = useApp();
  const changes = diffConfig(before, after);
  if (changes.length === 0) {
    return <span className="mut">{app.t("No changes since you opened the editor.", "自打开编辑器起没有改动。")}</span>;
  }
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      {changes.map((c, i) => {
        const label = labelForPath(c.path);
        return (
          <div key={i} className="row" style={{ alignItems: "baseline", gap: 8 }}>
            <Pill tone={TONE[c.kind]}>{c.kind}</Pill>
            <strong>{label}</strong>
            {label !== c.path && (
              <span className="mut mono" style={{ fontSize: 11 }}>{c.path}</span>
            )}
            <span className="mut mono" style={{ fontSize: 12 }}>
              {short(c.before)} → {short(c.after)}
            </span>
          </div>
        );
      })}
    </div>
  );
}
