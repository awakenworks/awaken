import type {
  InputBinding,
  InputResourceKind,
  ResourceAccess,
  ResourceInputDefaultMounts,
} from "./api/types";

function defaultAccess(kind: InputResourceKind): ResourceAccess {
  return kind === "memory_store" ? "read_write" : "read_only";
}

/** Create an authoring candidate only from the server's read-only projection. */
export function createDefaultResourceBinding(
  defaults: ResourceInputDefaultMounts | undefined,
  kind: InputResourceKind,
  bindingId: string,
  targetId: string,
): InputBinding | undefined {
  const mountPath = defaults?.[kind];
  if (!mountPath?.trim()) return undefined;
  return {
    binding_id: bindingId,
    target: { kind, id: targetId },
    mount_path: mountPath,
    access: defaultAccess(kind),
  };
}

/** Change kind without mutating the durable row or inventing a local default. */
export function switchResourceBindingKind(
  binding: InputBinding,
  defaults: ResourceInputDefaultMounts | undefined,
  kind: InputResourceKind,
): InputBinding | undefined {
  const candidate = createDefaultResourceBinding(defaults, kind, binding.binding_id, "");
  return candidate ? { ...binding, ...candidate } : undefined;
}
