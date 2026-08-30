import type { Locale } from "./app-state";

const STATUS_LABELS: Record<string, [string, string]> = {
  pending: ["Pending", "等待中"],
  running: ["Running", "运行中"],
  completed: ["Completed", "已完成"],
  failed: ["Failed", "失败"],
  canceled: ["Canceled", "已取消"],
  idle: ["Idle", "空闲"],
  rescheduling: ["Rescheduling", "正在重新调度"],
  terminated: ["Stopped", "已停止"],
  active: ["Active", "使用中"],
  archived: ["Archived", "已归档"],
  valid: ["Valid", "有效"],
  invalid: ["Invalid", "无效"],
  unknown: ["Not verified", "尚未验证"],
};

const CREDENTIAL_KIND_LABELS: Record<string, [string, string]> = {
  vault: ["Stored secret", "已保存密钥"],
  worker_local: ["Worker-provided", "Worker 提供"],
  environment: ["Environment variable", "环境变量"],
};

function label(table: Record<string, [string, string]>, value: string, locale: Locale): string {
  return table[value]?.[locale === "zh" ? 1 : 0]
    ?? value.replaceAll("_", " ").replace(/^./, (first) => first.toUpperCase());
}

export function statusLabel(value: string, locale: Locale): string {
  return label(STATUS_LABELS, value, locale);
}

export function credentialKindLabel(value: string, locale: Locale): string {
  return label(CREDENTIAL_KIND_LABELS, value, locale);
}

const IDENTIFIER_WORDS: Record<string, string> = {
  acp: "ACP",
  ai: "AI",
  api: "API",
  cli: "CLI",
  codex: "Codex",
  deepseek: "DeepSeek",
  http: "HTTP",
  mcp: "MCP",
  mgmt: "Management",
  openai: "OpenAI",
  preview: "Agent preview",
  sdk: "SDK",
  svc: "Service",
  ui: "UI",
};

function withoutOpaqueSuffix(value: string): string {
  const uuid = value.match(/^(.+?)-[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i);
  if (uuid) return uuid[1];
  const generated = value.match(/^(.+?)-(?:[0-9]{10,}|[0-9a-f]{16,})$/i);
  return generated?.[1] ?? value;
}

/** Turn a user-chosen slug into a readable fallback label. Opaque generated
 * identifiers stay behind Technical ID affordances and must not use this path. */
export function identifierLabel(value: string): string {
  const words = withoutOpaqueSuffix(value.trim()).split(/[._-]+/).filter(Boolean);
  if (!words.length) return "";
  return words.map((word, index) => {
    const known = IDENTIFIER_WORDS[word.toLowerCase()];
    if (known) return known;
    const lower = word.toLowerCase();
    return index === 0 ? lower.replace(/^./, (first) => first.toUpperCase()) : lower;
  }).join(" ");
}

export function sessionDisplayTitle(
  title: string | null | undefined,
  agentId: string | null | undefined,
  locale: Locale,
): string {
  const fallback = agentId?.startsWith("preview-")
    ? locale === "zh" ? "Agent 预览" : "Agent preview"
    : locale === "zh" ? "未命名会话" : "Untitled Session";
  return entityDisplayTitle(title, fallback);
}

/** Preferred human label for an entity. Generated ids never become the primary
 * name merely because optional display metadata is absent. */
export function entityDisplayName(preferred: string | null | undefined, fallback: string): string {
  const value = preferred?.trim();
  if (!value) return fallback;
  return !value.includes(" ") && /[._-]/.test(value) ? identifierLabel(value) : value;
}

/** Collapse mechanically composed titles such as `Assistant · Assistant`
 * without rewriting meaningful multi-part task names. */
export function entityDisplayTitle(preferred: string | null | undefined, fallback: string): string {
  const title = entityDisplayName(preferred, fallback);
  return title.split(/\s*·\s*/).filter((part, index, parts) => (
    index === 0 || part.localeCompare(parts[index - 1], undefined, { sensitivity: "accent" }) !== 0
  )).join(" · ");
}

export function dateTimeLabel(value: string | null | undefined, locale: Locale): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? value
    : new Intl.DateTimeFormat(locale === "zh" ? "zh-CN" : "en-US", {
        dateStyle: "medium",
        timeStyle: "short",
      }).format(date);
}
