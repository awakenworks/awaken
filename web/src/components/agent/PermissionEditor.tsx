// The permission policy editor: a default decision (Allow/Ask/Deny) plus an
// ordered rule table (glob pattern → behavior). Authors the agent's `permission`
// config section, which the runtime turns into the thread's authorization gate
// (deny is absolute; otherwise the most specific match wins; unmatched → default).
// Reuses Segmented for the decision toggles — no bespoke widgets.

import { Button, Segmented } from "../ui";
import type { PermissionBehavior, PermissionConfig } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

const BEHAVIORS: { value: PermissionBehavior; label: string; zh: string }[] = [
  { value: "allow", label: "Allow", zh: "允许" },
  { value: "ask", label: "Ask", zh: "询问" },
  { value: "deny", label: "Deny", zh: "拒绝" },
];

export default function PermissionEditor({
  value,
  onChange,
}: {
  value: PermissionConfig;
  onChange: (next: PermissionConfig) => void;
}) {
  const app = useApp();
  const rules = value.rules ?? [];
  const behaviorOpts = BEHAVIORS.map((b) => ({ value: b.value, label: app.t(b.label, b.zh) }));

  const setRule = (i: number, patch: Partial<{ pattern: string; behavior: PermissionBehavior }>) =>
    onChange({ ...value, rules: rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
  const addRule = () => onChange({ ...value, rules: [...rules, { pattern: "", behavior: "ask" }] });
  const removeRule = (i: number) => onChange({ ...value, rules: rules.filter((_, j) => j !== i) });

  return (
    <div className="permission-editor" style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="field">
        <label>{app.t("Default decision", "默认裁决")}</label>
        <span className="mut">
          {app.t("Applied to any tool call no rule matches.", "用于任何规则未命中的工具调用。")}
        </span>
        <Segmented
          options={behaviorOpts}
          value={value.default_behavior ?? "ask"}
          onChange={(b) => onChange({ ...value, default_behavior: b })}
        />
      </div>

      <div className="field">
        <label>{app.t("Rules", "规则")}</label>
        <span className="mut">
          {app.t(
            'Deny always wins; otherwise the most specific match decides. e.g. bash(command ~ "*rm -rf*") → Deny. Tool ids are lowercase (bash/read/write).',
            'Deny 始终优先;否则最具体的匹配胜出。如 bash(command ~ "*rm -rf*") → 拒绝。工具名小写(bash/read/write)。',
          )}
        </span>
        {rules.map((r, i) => (
          <div className="row" key={i} style={{ alignItems: "center" }}>
            <input
              className="input mono"
              style={{ flex: 1 }}
              placeholder='bash(command ~ "*rm -rf*")'
              value={r.pattern}
              onChange={(e) => setRule(i, { pattern: e.target.value })}
            />
            <Segmented
              options={behaviorOpts}
              value={r.behavior}
              onChange={(b) => setRule(i, { behavior: b })}
            />
            <Button variant="ghost" style={{ height: 26 }} onClick={() => removeRule(i)}>
              ✕
            </Button>
          </div>
        ))}
        {rules.length === 0 && (
          <span className="mut">
            {app.t("No rules — every call uses the default.", "无规则——全部走默认裁决。")}
          </span>
        )}
        <Button style={{ alignSelf: "flex-start" }} onClick={addRule}>
          + {app.t("add rule", "添加规则")}
        </Button>
      </div>
    </div>
  );
}
