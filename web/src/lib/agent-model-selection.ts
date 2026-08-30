import type { ModelSelection, RuntimeCap } from "./api/types";

export interface AcpModelChoice {
  key: string;
  label: string;
  selection: Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>;
}

export function isAcpModelSelection(model: ModelSelection | { id: string } | string): boolean {
  return typeof model === "object" && "mode" in model
    && (model.mode === "backend_default" || model.mode === "backend_exact");
}

export function executionRuntimeId(model: ModelSelection | { id: string } | string): string {
  return isAcpModelSelection(model)
    ? (model as Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>).backend_ref
    : "awaken";
}

export function defaultSelectionForRuntime(runtime: RuntimeCap): AcpModelChoice["selection"] | null {
  return acpModelChoices([runtime])[0]?.selection ?? null;
}

export function availableAcpRuntimes(runtimes: RuntimeCap[]): RuntimeCap[] {
  return runtimes.filter(
    (runtime) => runtime.kind === "acp" && runtime.local?.login_state === "available",
  );
}

export function acpModelChoices(runtimes: RuntimeCap[]): AcpModelChoice[] {
  return availableAcpRuntimes(runtimes).flatMap((runtime) => {
    const defaults: AcpModelChoice[] = [{
      key: `${runtime.id}:default`,
      label: `Use ${runtime.label} default`,
      selection: { mode: "backend_default", backend_ref: runtime.id },
    }];
    const models = runtime.local?.negotiated?.config_options
      .find((option) => option.native_id === "model")
      ?.choices.map((choice) => ({
        key: `${runtime.id}:${choice.native_value}`,
        label: `${runtime.label}: ${choice.name}`,
        selection: {
          mode: "backend_exact" as const,
          backend_ref: runtime.id,
          model_ref: choice.native_value,
        },
      })) ?? [];
    return [...defaults, ...models];
  });
}
