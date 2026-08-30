import {
  SchemaForm as SharedSchemaForm,
  stringControlForSchema,
  type JsonSchema,
  type SchemaFormProps as SharedSchemaFormProps,
} from "@awaken/ui";
import { useMemo } from "react";
import { useApp, type Locale } from "../../lib/app-state";

export type { JsonSchema };
export { stringControlForSchema };

export type SchemaFormProps = Omit<SharedSchemaFormProps, "classes" | "labels">;

const SCHEMA_ZH: Record<string, string> = {
  "Compaction Agent": "压缩 Agent",
  "Published auxiliary Agent id. Defaults to compactor.": "已发布的辅助 Agent ID；默认使用 compactor。",
  "Compactor system instructions": "压缩 Agent 系统指令",
  "Per-main-Agent override for the selected compactor's system instructions.": "为当前主 Agent 覆盖所选压缩 Agent 的系统指令。",
  "Message-count trigger (fallback when max_tokens is unset).": "按消息数触发；未设置 max_tokens 时作为后备条件。",
  "Keep this many most-recent messages verbatim; the summary covers the rest.": "原样保留最近这些消息，其余内容由摘要覆盖。",
  "The model's context window in tokens; enables the token-aware trigger.": "模型上下文窗口的 Token 数；设置后启用按 Token 触发。",
  "Fold once estimated context reaches this fraction of max_tokens (e.g. 0.8).": "预计上下文达到 max_tokens 的该比例时执行压缩（例如 0.8）。",
  "Start non-blocking background compaction at this fraction of the hard threshold.": "达到硬触发阈值的该比例时，在后台非阻塞地预先压缩。",
  "Compaction instructions": "压缩指令",
  "Compaction prompt: what the summary should preserve. Blank uses the built-in default.": "压缩 Prompt：指定摘要必须保留的内容；留空使用内置默认值。",
  "Automatic memory binding": "自动记忆绑定",
  "Exact Session MemoryStore binding used by the optional Awaken recall/extraction extension. Null leaves standard mounts available without hidden memory behavior.": "可选记忆召回/提取扩展使用的精确 Session MemoryStore 绑定。留空时仍可使用标准挂载，不会启用隐藏记忆行为。",
  "Enable request-only Memory recall for this Agent.": "为此 Agent 启用仅作用于当前请求的记忆召回。",
  "Enable terminal Memory extraction for this Agent when the binding is writable.": "绑定可写时，为此 Agent 启用运行结束后的记忆提取。",
  "Truncate each memory to this many characters (0 = unbounded).": "每条记忆最多保留这些字符（0 表示不限制）。",
  "Cap the whole recall block to this many characters.": "整个召回内容块最多包含这些字符。",
  "Inject at most this many memories.": "最多注入这些记忆条目。",
  "Use relevance selection once the store holds more than this many memories.": "记忆库条目超过该数量后启用相关性筛选。",
  "Memory extraction Agent": "记忆提取 Agent",
  "Published auxiliary Agent id. Null uses memory-extractor.": "已发布的辅助 Agent ID；留空使用 memory-extractor。",
  "Memory extraction instructions": "记忆提取指令",
  "Memory-extraction system prompt: what durable facts to save or ignore. Blank uses the built-in taxonomy.": "记忆提取系统 Prompt：指定要保存或忽略的持久事实；留空使用内置分类规则。",
  "Extraction task prompt": "提取任务 Prompt",
  "Task prompt appended when a completed step is handed to the background memory extractor.": "完成的步骤交给后台记忆提取器时附加的任务 Prompt。",
  "Memory recall selector Agent": "记忆召回筛选 Agent",
  "Published auxiliary Agent id. Null uses memory-selector.": "已发布的辅助 Agent ID；留空使用 memory-selector。",
  "Recall selector instructions": "召回筛选指令",
  "Per-Agent override for the selected recall Agent's system instructions.": "为当前 Agent 覆盖所选召回 Agent 的系统指令。",
  "Canonical ids of existing ordinary tools that may be invoked in the background. This does not grant the tools or change their permission, concurrency, recovery, or resource policies.": "允许后台调用的已有普通工具规范 ID。此配置不会授予工具，也不会改变其权限、并发、恢复或资源策略。",
};

const SCHEMA_FIELD_ZH: Record<string, string> = {
  agent_id: "Agent ID",
  system_prompt: "系统指令",
  threshold: "消息数阈值",
  keep_last: "原样保留消息数",
  max_tokens: "最大 Token 数",
  trigger_ratio: "触发比例",
  prefetch_ratio: "预压缩比例",
  prompt: "Prompt",
  binding_id: "记忆绑定 ID",
  recall_enabled: "启用记忆召回",
  extraction_enabled: "启用记忆提取",
  max_chars_per_memory: "单条记忆最大字符数",
  max_total_chars: "召回内容最大字符数",
  max_memories: "最大记忆条目数",
  relevance_threshold: "相关性筛选阈值",
  extractor_agent_id: "记忆提取 Agent ID",
  extractor_system_prompt: "记忆提取系统指令",
  extraction_task_prompt: "提取任务 Prompt",
  selector_agent_id: "召回筛选 Agent ID",
  selector_system_prompt: "召回筛选系统指令",
  tools: "允许后台执行的工具 ID",
};

export function localizeSchema(schema: JsonSchema, locale: Locale): JsonSchema {
  if (locale !== "zh") return schema;
  const translate = (value: unknown, parentKey?: string): unknown => {
    if (typeof value === "string") return SCHEMA_ZH[value] ?? value;
    if (Array.isArray(value)) return value.map((entry) => translate(entry));
    if (value && typeof value === "object") {
      const translated = Object.fromEntries(
        Object.entries(value).map(([key, entry]) => [key, translate(entry, key)]),
      );
      if (parentKey && SCHEMA_FIELD_ZH[parentKey] && !("title" in translated)) {
        translated.title = SCHEMA_FIELD_ZH[parentKey];
      }
      return translated;
    }
    return value;
  };
  return translate(schema) as JsonSchema;
}

/** Awaken visual/copy adapter over the shared JSON Schema renderer. */
export function SchemaForm(props: SchemaFormProps) {
  const app = useApp();
  const schema = useMemo(() => localizeSchema(props.schema, app.locale), [props.schema, app.locale]);
  return (
    <SharedSchemaForm
      {...props}
      schema={schema}
      classes={{
        button: "btn ghost",
        error: "err",
        field: "field",
        input: "input",
        mono: "mono",
        muted: "mut",
        object: "schema-object",
        root: "schema-form",
        row: "row",
      }}
      labels={{
        addItem: app.t("+ add", "+ 添加"),
        invalidJson: app.t("invalid JSON — not saved", "JSON 非法 — 未保存"),
        removeItem: app.t("Remove item", "移除条目"),
      }}
    />
  );
}
