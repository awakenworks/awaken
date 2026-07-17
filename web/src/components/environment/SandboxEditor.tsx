// The sandbox knobs for an environment whose worker runs isolated (bwrap). Two
// layers, elegant-by-reuse: named PRESETS (one click for the common cases, from
// `/v1/capabilities` `sandbox.presets`) over a schema-driven form (`SchemaForm`
// rendering `sandbox.config_schema` — the SAME schema the assistant authors from,
// so nothing here is bespoke). Isolation / mounts / egress / limits map 1:1 onto
// the backend `SandboxSpec`. Progressive disclosure: this whole block only shows
// when the environment's Placement is "sandboxed".

import { SchemaForm, type JsonSchema, Button } from "../ui";
import { useApp } from "../../lib/app-state";
import type { SandboxCapability, SandboxConfig } from "../../lib/api/types";

export function SandboxEditor({
  value,
  onChange,
  sandbox,
}: {
  value: SandboxConfig;
  onChange: (v: SandboxConfig) => void;
  sandbox: SandboxCapability;
}) {
  const app = useApp();
  const activePreset = sandbox.presets.find(
    (p) => JSON.stringify(p.spec) === JSON.stringify(value),
  )?.id;

  return (
    <div className="field" style={{ gap: 8 }}>
      <label>{app.t("Sandbox", "沙箱")}</label>
      <span className="mut">
        {app.t(
          "Start from a preset, then adjust. Isolation, mounts, egress and limits map to the worker's SandboxSpec.",
          "从预设起步再微调。隔离、挂载、egress、资源限额对应 worker 的 SandboxSpec。",
        )}
      </span>
      <div className="row" style={{ flexWrap: "wrap", gap: 6 }}>
        {sandbox.presets.map((p) => (
          <Button
            key={p.id}
            variant={activePreset === p.id ? "primary" : "ghost"}
            style={{ height: 26 }}
            title={p.description}
            onClick={() => onChange(p.spec)}
          >
            {p.label}
          </Button>
        ))}
      </div>
      <SchemaForm
        schema={sandbox.config_schema as JsonSchema}
        value={value}
        onChange={(v) => onChange((v ?? {}) as SandboxConfig)}
      />
    </div>
  );
}
