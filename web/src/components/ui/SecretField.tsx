import {
  SecretField as SharedSecretField,
  type SecretIntent,
  type SecretMode,
} from "@awaken/ui";
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
  return (
    <SharedSecretField
      classes={{
        activeMode: "primary",
        inactiveMode: "ghost",
        input: "input mono",
        modeButton: "btn",
        modes: "row",
        root: "field",
        status: "mut",
      }}
      hasStored={hasStored}
      label={label}
      labels={{
        clear: app.t("Clear", "清除"),
        cleared: app.t("Stored secret will be removed.", "将移除已存密钥。"),
        keep: app.t("Keep", "保留"),
        kept: app.t("Stored secret unchanged.", "保留已存密钥不变。"),
        placeholder: app.t("enter new secret…", "输入新密钥…"),
        replace: app.t("Replace", "替换"),
      }}
      onChange={onChange}
      {...(placeholder === undefined ? {} : { placeholder })}
    />
  );
}
