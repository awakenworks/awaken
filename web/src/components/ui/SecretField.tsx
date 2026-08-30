import { useId, useState } from "react";
import type { SecretIntent, SecretMode } from "@awaken/ui";
import { useApp } from "../../lib/app-state";

export type { SecretIntent, SecretMode };

export function SecretField({
  label,
  hasStored,
  onChange,
  placeholder,
}: {
  readonly label: string;
  readonly hasStored: boolean;
  readonly onChange: (intent: SecretIntent) => void;
  readonly placeholder?: string;
}) {
  const app = useApp();
  const inputId = useId();
  const [mode, setMode] = useState<SecretMode>(hasStored ? "keep" : "replace");
  const [value, setValue] = useState("");
  const pick = (next: SecretMode) => {
    setMode(next);
    onChange(next === "replace" ? { mode: "replace", value } : { mode: next });
  };
  return (
    <div className="field">
      <label htmlFor={inputId}>{label}</label>
      {hasStored && (
        <span className="row" role="group" aria-label={app.t("Secret update mode", "密钥更新方式")}>
          {(["keep", "replace", "clear"] as const).map((candidate) => (
            <button
              className={`btn ${mode === candidate ? "primary" : "ghost"}`}
              key={candidate}
              onClick={() => pick(candidate)}
              type="button"
            >
              {candidate === "keep"
                ? app.t("Keep", "保留")
                : candidate === "replace"
                  ? app.t("Replace", "替换")
                  : app.t("Clear", "清除")}
            </button>
          ))}
        </span>
      )}
      {mode === "replace" ? (
        <input
          autoComplete="off"
          className="input mono"
          id={inputId}
          onChange={(event) => {
            setValue(event.target.value);
            onChange({ mode: "replace", value: event.target.value });
          }}
          placeholder={placeholder ?? app.t("enter new secret…", "输入新密钥…")}
          type="password"
          value={value}
        />
      ) : (
        <span className="mut">
          {mode === "keep"
            ? app.t("Stored secret unchanged.", "保留已存密钥不变。")
            : app.t("Stored secret will be removed.", "将移除已存密钥。")}
        </span>
      )}
    </div>
  );
}
