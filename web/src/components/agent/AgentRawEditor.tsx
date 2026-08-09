// Lossless escape hatch modelled after Oversight's workflow JSON view: every field
// round-trips through the same Agent object instead of being hidden behind a partial
// form. Friendly sections remain the default; valid JSON updates the draft live.

import { useState } from "react";
import type { AgentConfig } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

export default function AgentRawEditor({
  value,
  onChange,
  onValidityChange,
}: {
  value: AgentConfig;
  onChange: (value: AgentConfig) => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState(() => JSON.stringify(value, null, 2));
  const [error, setError] = useState("");
  return (
    <div className="agent-raw-editor">
      <div className="banner info">
        <span>{"{}"}</span>
        <span>{app.t("Lossless Agent object view. Valid edits update the same current draft used by Try, Save draft, Validate, and Publish.", "无损 Agent 对象视图。有效修改会更新试运行、保存草稿、校验与发布共同使用的当前草稿。")}</span>
      </div>
      <textarea
        aria-label={app.t("Agent JSON", "Agent JSON")}
        className="input mono"
        value={draft}
        onChange={(event) => {
          const raw = event.target.value;
          setDraft(raw);
          try {
            const parsed = JSON.parse(raw) as AgentConfig;
            if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("object required");
            onChange(parsed);
            onValidityChange(true);
            setError("");
          } catch {
            onValidityChange(false);
            setError(app.t("Invalid Agent JSON — draft actions are disabled until it parses.", "Agent JSON 非法——解析成功前草稿操作不可用。"));
          }
        }}
      />
      {error && <div className="err" role="alert">{error}</div>}
    </div>
  );
}
