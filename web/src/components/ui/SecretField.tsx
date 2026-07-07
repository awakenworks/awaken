// Tri-state secret input for editing a stored credential without ever reading it
// back: "keep" (leave the stored secret untouched), "replace" (enter a new value),
// or "clear" (remove it). The value is write-only — the current secret is never
// shown. Emits the operator's intent so the caller sends the right patch.

import { useState } from "react";
import { useApp } from "../../lib/app-state";

export type SecretMode = "keep" | "replace" | "clear";
export interface SecretIntent {
  mode: SecretMode;
  /** Present only when mode === "replace". */
  value?: string;
}

export function SecretField({
  label,
  hasStored,
  onChange,
  placeholder,
}: {
  label: string;
  /** Whether a secret is already stored (enables "keep" / "clear"). */
  hasStored: boolean;
  onChange: (intent: SecretIntent) => void;
  placeholder?: string;
}) {
  const app = useApp();
  const [mode, setMode] = useState<SecretMode>(hasStored ? "keep" : "replace");
  const [value, setValue] = useState("");

  const pick = (m: SecretMode) => {
    setMode(m);
    onChange(m === "replace" ? { mode: "replace", value } : { mode: m });
  };

  const modes: SecretMode[] = hasStored ? ["keep", "replace", "clear"] : ["replace"];
  const modeLabel: Record<SecretMode, [string, string]> = {
    keep: ["Keep", "保留"],
    replace: ["Replace", "替换"],
    clear: ["Clear", "清除"],
  };

  return (
    <div className="field">
      <label>{label}</label>
      {hasStored && (
        <div className="row">
          {modes.map((m) => (
            <button
              key={m}
              type="button"
              className={`btn ${mode === m ? "primary" : "ghost"}`}
              style={{ height: 26 }}
              onClick={() => pick(m)}
            >
              {app.t(...modeLabel[m])}
            </button>
          ))}
        </div>
      )}
      {mode === "replace" && (
        <input
          className="input mono"
          type="password"
          autoComplete="off"
          placeholder={placeholder ?? app.t("enter new secret…", "输入新密钥…")}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            onChange({ mode: "replace", value: e.target.value });
          }}
        />
      )}
      {mode === "keep" && <span className="mut">{app.t("Stored secret unchanged.", "保留已存密钥不变。")}</span>}
      {mode === "clear" && <span className="mut">{app.t("Stored secret will be removed.", "将移除已存密钥。")}</span>}
    </div>
  );
}
