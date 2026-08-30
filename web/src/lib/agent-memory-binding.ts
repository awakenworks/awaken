import type { InputBinding } from "./api/types";
import type { AgentConfig } from "./api/types";

export function shouldEnableMemoryExtraction(
  previous: InputBinding[],
  next: InputBinding[],
  enabledPlugins: string[],
): boolean {
  const hadStore = previous.some((binding) => binding.target.kind === "memory_store");
  const hasStore = next.some((binding) => binding.target.kind === "memory_store");
  return !hadStore && hasStore && !enabledPlugins.includes("memory");
}

/** Keep the Memory extension pinned to one explicit mounted binding. Runtime
 * refuses to guess when multiple stores exist, so authoring must commit the
 * selected binding together with the resource edit. */
export function reconcileMemoryBinding(
  previous: InputBinding[],
  next: InputBinding[],
  config: Pick<AgentConfig, "plugins" | "plugin_config">,
): Partial<Pick<AgentConfig, "plugins" | "plugin_config">> {
  const stores = next.filter((binding) => binding.target.kind === "memory_store");
  const currentMemory = (config.plugin_config?.memory ?? {}) as Record<string, unknown>;
  const currentBinding = typeof currentMemory.binding_id === "string" ? currentMemory.binding_id : "";
  const selected = stores.some((binding) => binding.binding_id === currentBinding)
    ? currentBinding
    : stores[0]?.binding_id ?? "";
  const enable = shouldEnableMemoryExtraction(previous, next, config.plugins);
  const plugins = enable ? [...config.plugins, "memory"] : config.plugins;
  if (!selected && !currentBinding && !enable) return {};
  const memory = { ...currentMemory };
  if (selected) memory.binding_id = selected;
  else delete memory.binding_id;
  return {
    plugins,
    plugin_config: { ...config.plugin_config, memory },
  };
}
