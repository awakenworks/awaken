import type { InputBinding } from "./api/types";

export function shouldEnableMemoryExtraction(
  previous: InputBinding[],
  next: InputBinding[],
  enabledPlugins: string[],
): boolean {
  const hadStore = previous.some((binding) => binding.target.kind === "memory_store");
  const hasStore = next.some((binding) => binding.target.kind === "memory_store");
  return !hadStore && hasStore && !enabledPlugins.includes("memory");
}
