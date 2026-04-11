export type PermissionBehavior = "allow" | "ask" | "deny";
export type PermissionScope = "once" | "session" | "thread" | "project" | "user";

export interface PermissionRuleConfig {
  tool: string;
  behavior: PermissionBehavior;
  scope: PermissionScope;
}

export interface PermissionConfig {
  default_behavior: PermissionBehavior;
  rules: PermissionRuleConfig[];
}

export interface GenerativeUiConfig {
  catalog_id: string;
  examples: string;
  instructions: string;
}

export type ReminderStatus = "any" | "success" | "error" | "pending";
export type ReminderFieldOp =
  | "glob"
  | "exact"
  | "regex"
  | "not_glob"
  | "not_exact"
  | "not_regex";
export type ReminderTarget =
  | "system"
  | "suffix_system"
  | "session"
  | "conversation";
export type ReminderMode =
  | "any"
  | "status"
  | "content_text"
  | "content_fields"
  | "status_and_text"
  | "status_and_fields";

export interface ReminderFieldConfig {
  path: string;
  op: ReminderFieldOp;
  value: string;
}

export interface ReminderRuleDraft {
  name: string;
  tool: string;
  mode: ReminderMode;
  status: ReminderStatus;
  text: string;
  fields: ReminderFieldConfig[];
  target: ReminderTarget;
  content: string;
  cooldown_turns: number;
}

export interface ReminderConfigDraft {
  rules: ReminderRuleDraft[];
}

export function pluginConfigEntryKey(pluginId: string, schemaKey: string): string {
  return `${pluginId}:${schemaKey}`;
}

export function pluginDisplayName(pluginId: string): string {
  switch (pluginId) {
    case "permission":
      return "Permissions";
    case "reminder":
      return "Reminders";
    case "generative-ui":
      return "Generative UI";
    case "ext-deferred-tools":
      return "Deferred Tools";
    case "frontend_tools":
      return "Frontend Tools";
    default:
      return pluginId;
  }
}

export function pluginConfigDisplaySummary(
  pluginId: string,
  schemaKey: string,
  value: unknown,
): string {
  if (schemaKey === "permission" || pluginId === "permission") {
    const config = normalizePermissionConfig(value);
    return `Default ${config.default_behavior} · ${config.rules.length} rule${config.rules.length === 1 ? "" : "s"}`;
  }

  if (schemaKey === "reminder" || pluginId === "reminder") {
    const config = normalizeReminderConfig(value);
    return config.rules.length === 0
      ? "No reminder rules"
      : `${config.rules.length} reminder rule${config.rules.length === 1 ? "" : "s"}`;
  }

  if (schemaKey === "generative-ui" || pluginId === "generative-ui") {
    const config = normalizeGenerativeUiConfig(value);
    const parts = [];
    if (config.instructions.trim()) {
      parts.push("instruction override");
    }
    if (config.catalog_id.trim()) {
      parts.push("catalog override");
    }
    if (config.examples.trim()) {
      parts.push("examples");
    }
    return parts.length === 0 ? "Prompt defaults" : parts.join(" · ");
  }

  if (hasMeaningfulConfig(value)) {
    return "Configured";
  }

  return "Schema form";
}

export function schemaTitle(schema: Record<string, unknown>): string | null {
  const title = schema.title;
  return typeof title === "string" && title.trim() ? title : null;
}

export function schemaDescription(schema: Record<string, unknown>): string | null {
  const description = schema.description;
  return typeof description === "string" && description.trim() ? description : null;
}

export function normalizePermissionConfig(value: unknown): PermissionConfig {
  const record = asRecord(value);
  const rules = asArray(record.rules).map((rule) => {
    const next = asRecord(rule);
    return {
      tool: asString(next.tool),
      behavior: isPermissionBehavior(next.behavior) ? next.behavior : "ask",
      scope: isPermissionScope(next.scope) ? next.scope : "project",
    };
  });

  return {
    default_behavior: isPermissionBehavior(record.default_behavior)
      ? record.default_behavior
      : "ask",
    rules,
  };
}

export function serializePermissionConfig(config: PermissionConfig): Record<string, unknown> {
  return {
    default_behavior: config.default_behavior,
    rules: config.rules.map((rule) => ({
      tool: rule.tool,
      behavior: rule.behavior,
      scope: rule.scope,
    })),
  };
}

export function createPermissionRule(): PermissionRuleConfig {
  return {
    tool: "",
    behavior: "ask",
    scope: "project",
  };
}

export function normalizeGenerativeUiConfig(value: unknown): GenerativeUiConfig {
  const record = asRecord(value);
  return {
    catalog_id: asString(record.catalog_id),
    examples: asString(record.examples),
    instructions: asString(record.instructions),
  };
}

export function serializeGenerativeUiConfig(
  config: GenerativeUiConfig,
): Record<string, unknown> {
  const next: Record<string, unknown> = {};
  if (config.catalog_id.trim()) {
    next.catalog_id = config.catalog_id.trim();
  }
  if (config.examples.trim()) {
    next.examples = config.examples;
  }
  if (config.instructions.trim()) {
    next.instructions = config.instructions;
  }
  return next;
}

export function normalizeReminderConfig(value: unknown): ReminderConfigDraft {
  const record = asRecord(value);
  return {
    rules: asArray(record.rules).map(normalizeReminderRule),
  };
}

export function serializeReminderConfig(
  config: ReminderConfigDraft,
): Record<string, unknown> {
  return {
    rules: config.rules.map((rule) => ({
      name: rule.name.trim() ? rule.name.trim() : undefined,
      tool: rule.tool,
      output: serializeReminderOutput(rule),
      message: {
        target: rule.target,
        content: rule.content,
        cooldown_turns: rule.cooldown_turns,
      },
    })),
  };
}

export function createReminderRule(): ReminderRuleDraft {
  return {
    name: "",
    tool: "",
    mode: "any",
    status: "success",
    text: "",
    fields: [],
    target: "system",
    content: "",
    cooldown_turns: 0,
  };
}

function normalizeReminderRule(value: unknown): ReminderRuleDraft {
  const record = asRecord(value);
  const message = asRecord(record.message);
  const output = record.output;
  const outputRecord = asRecord(output);

  let mode: ReminderMode = "any";
  let status: ReminderStatus = "success";
  let text = "";
  let fields: ReminderFieldConfig[] = [];

  if (typeof output === "string") {
    mode = output === "any" ? "any" : "content_text";
    if (mode === "content_text") {
      text = output;
    }
  } else if ("status" in outputRecord || "content" in outputRecord) {
    if (isReminderStatus(outputRecord.status)) {
      status = outputRecord.status;
    }

    const content = outputRecord.content;
    if (typeof content === "string") {
      text = content;
      mode = "status" in outputRecord ? "status_and_text" : "content_text";
    } else if ("fields" in asRecord(content)) {
      fields = asArray(asRecord(content).fields).map((field) => {
        const next = asRecord(field);
        return {
          path: asString(next.path),
          op: isReminderFieldOp(next.op) ? next.op : "glob",
          value: asString(next.value),
        };
      });
      mode = "status" in outputRecord ? "status_and_fields" : "content_fields";
    } else if ("status" in outputRecord) {
      mode = "status";
    }
  }

  return {
    name: asString(record.name),
    tool: asString(record.tool),
    mode,
    status,
    text,
    fields,
    target: isReminderTarget(message.target) ? message.target : "system",
    content: asString(message.content),
    cooldown_turns: asNumber(message.cooldown_turns, 0),
  };
}

function serializeReminderOutput(rule: ReminderRuleDraft): unknown {
  switch (rule.mode) {
    case "any":
      return "any";
    case "status":
      return { status: rule.status };
    case "content_text":
      return { content: rule.text };
    case "content_fields":
      return {
        content: {
          fields: rule.fields.map((field) => ({
            path: field.path,
            op: field.op,
            value: field.value,
          })),
        },
      };
    case "status_and_text":
      return {
        status: rule.status,
        content: rule.text,
      };
    case "status_and_fields":
      return {
        status: rule.status,
        content: {
          fields: rule.fields.map((field) => ({
            path: field.path,
            op: field.op,
            value: field.value,
          })),
        },
      };
  }
}

function hasMeaningfulConfig(value: unknown): boolean {
  const record = asRecord(value);
  if (Object.keys(record).length === 0) {
    return false;
  }

  return Object.values(record).some((item) => {
    if (typeof item === "string") {
      return item.trim().length > 0;
    }
    if (Array.isArray(item)) {
      return item.length > 0;
    }
    if (item && typeof item === "object") {
      return Object.keys(asRecord(item)).length > 0;
    }
    return item !== undefined && item !== null;
  });
}

function isPermissionBehavior(value: unknown): value is PermissionBehavior {
  return value === "allow" || value === "ask" || value === "deny";
}

function isPermissionScope(value: unknown): value is PermissionScope {
  return (
    value === "once" ||
    value === "session" ||
    value === "thread" ||
    value === "project" ||
    value === "user"
  );
}

function isReminderStatus(value: unknown): value is ReminderStatus {
  return (
    value === "any" ||
    value === "success" ||
    value === "error" ||
    value === "pending"
  );
}

function isReminderFieldOp(value: unknown): value is ReminderFieldOp {
  return (
    value === "glob" ||
    value === "exact" ||
    value === "regex" ||
    value === "not_glob" ||
    value === "not_exact" ||
    value === "not_regex"
  );
}

function isReminderTarget(value: unknown): value is ReminderTarget {
  return (
    value === "system" ||
    value === "suffix_system" ||
    value === "session" ||
    value === "conversation"
  );
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function asNumber(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

export function moveItem<T>(items: T[], fromIndex: number, toIndex: number): T[] {
  const next = [...items];
  const [item] = next.splice(fromIndex, 1);
  next.splice(toIndex, 0, item);
  return next;
}

export function createReminderField(): ReminderFieldConfig {
  return {
    path: "",
    op: "glob",
    value: "",
  };
}
