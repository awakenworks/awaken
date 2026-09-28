// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Secret entry is a plaintext browser boundary; masking is not proof of custody or
// @formal reason: isolation.
import { SecretField as SharedSecretField, type SecretIntent, type SecretMode } from "@awaken/ui";
import { useApp } from "../../lib/app-state";

export type { SecretIntent, SecretMode };

/** Product copy adapter. The shared field owns the write-only draft and modes. */
export function SecretField(props: {
  readonly label: string;
  readonly hasStored: boolean;
  readonly onChange: (intent: SecretIntent) => void;
  readonly placeholder?: string;
}) {
  const app = useApp();
  return <SharedSecretField {...props} labels={{
    modeLabel: app.t("Secret update mode", "密钥更新方式"),
    keep: app.t("Keep", "保留"),
    replace: app.t("Replace", "替换"),
    clear: app.t("Clear", "清除"),
    placeholder: app.t("enter new secret…", "输入新密钥…"),
    kept: app.t("Stored secret unchanged.", "保留已存密钥不变。"),
    cleared: app.t("Stored secret will be removed.", "将移除已存密钥。"),
  }} />;
}
