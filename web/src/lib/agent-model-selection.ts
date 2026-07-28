import type { ModelSelection, RuntimeCap } from "./api/types";

export interface AcpModelChoice {
  key: string;
  label: string;
  selection: Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>;
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
