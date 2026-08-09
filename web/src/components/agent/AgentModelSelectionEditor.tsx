import type { AgentConfig, ModelSelection, RuntimeCap } from "../../lib/api/types";
import { acpModelChoices, availableAcpRuntimes } from "../../lib/agent-model-selection";
import { useApp } from "../../lib/app-state";

interface Props {
  model: AgentConfig["model"];
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  onChange: (model: AgentConfig["model"]) => void;
  onManage: () => void;
}

function providerModelId(model: AgentConfig["model"]): string {
  if (typeof model === "string") return model;
  return "id" in model ? model.id : "";
}

function selectionValue(model: AgentConfig["model"]): string {
  if (typeof model === "string") return JSON.stringify({ id: model });
  if ("id" in model) return JSON.stringify({ id: model.id });
  return JSON.stringify(model);
}

export default function AgentModelSelectionEditor({
  model,
  readyModels,
  allModels,
  runtimes,
  onChange,
  onManage,
}: Props) {
  const app = useApp();
  const current = providerModelId(model);
  const providerModels =
    current && !readyModels.includes(current) ? [current, ...readyModels] : readyModels;
  const availableAcp = availableAcpRuntimes(runtimes);
  const choices = acpModelChoices(runtimes);
  const selected =
    typeof model === "object" && "mode" in model
      && (model.mode === "backend_default" || model.mode === "backend_exact")
      ? model
      : null;
  const runtime = selected
    ? availableAcp.find((candidate) => candidate.id === selected.backend_ref)
    : undefined;
  const setConfiguration = (
    configuration: NonNullable<
      Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>["configuration"]
    >,
  ) => {
    if (selected) onChange({ ...selected, configuration });
  };

  return (
    <>
      <label className="row" style={{ justifyContent: "space-between" }}>
        <span>{app.t("Model (references workspace catalog)", "模型(引用工作区 catalog)")}</span>
        <button className="manage-link" onClick={onManage}>
          {app.t("Manage ↗", "管理 ↗")}
        </button>
      </label>
      {providerModels.length > 0 || availableAcp.length > 0 ? (
        <select
          className="input mono"
          aria-label={app.t("Model (references workspace catalog)", "模型(引用工作区 catalog)")}
          value={selectionValue(model)}
          onChange={(event) => onChange(JSON.parse(event.target.value) as AgentConfig["model"])}
        >
          <option value="">{app.t("— select a model —", "— 选择模型 —")}</option>
          {providerModels.map((id) => (
            <option key={`provider:${id}`} value={selectionValue({ id })}>
              {id}{!readyModels.includes(id) ? app.t("  ⚠ no credential", "  ⚠ 无凭证") : ""}
            </option>
          ))}
          {choices.map((choice) => (
            <option key={choice.key} value={selectionValue(choice.selection)}>
              {choice.label}
            </option>
          ))}
        </select>
      ) : (
        <div className="banner gate">
          <span>◌</span>
          <span>{app.t(
            "No model has a credential yet. Configure a provider + key in Models (Manage ↗) — only credentialed models can be selected.",
            "还没有带凭证的模型。在 Models(管理 ↗)里配置一个 provider + key —— 只有带凭证的模型可选。",
          )}</span>
        </div>
      )}
      {selected && runtime?.local?.negotiated && (
        <div className="row" style={{ marginTop: 10, alignItems: "flex-start" }}>
          {runtime.local.negotiated.modes.length > 0 && (
            <div className="field" style={{ flex: 1 }}>
              <label>{app.t("ACP mode", "ACP 模式")}</label>
              <select
                className="input mono"
                value={selected.configuration?.mode ?? ""}
                onChange={(event) => setConfiguration({
                  ...(selected.configuration ?? {}),
                  mode: event.target.value || null,
                })}
              >
                <option value="">{app.t("CLI default", "CLI 默认")}</option>
                {runtime.local.negotiated.modes.map((mode) => (
                  <option key={mode.native_id} value={mode.native_id}>{mode.name}</option>
                ))}
              </select>
            </div>
          )}
          {runtime.local.negotiated.config_options
            .filter((option) => option.native_id !== "model")
            .map((option) => (
              <div className="field" style={{ flex: 1 }} key={option.native_id}>
                <label>{option.name}</label>
                <select
                  className="input mono"
                  value={selected.configuration?.options?.[option.native_id] ?? ""}
                  onChange={(event) => {
                    const options = { ...(selected.configuration?.options ?? {}) };
                    if (event.target.value) options[option.native_id] = event.target.value;
                    else delete options[option.native_id];
                    setConfiguration({ ...(selected.configuration ?? {}), options });
                  }}
                >
                  <option value="">{app.t("CLI default", "CLI 默认")}</option>
                  {option.choices.map((choice) => (
                    <option key={choice.native_value} value={choice.native_value}>
                      {choice.name}
                    </option>
                  ))}
                </select>
              </div>
            ))}
        </div>
      )}
      {allModels.length > readyModels.length && (
        <span className="mut" style={{ fontSize: 12 }}>
          {app.t(
            `${allModels.length - readyModels.length} model(s) hidden — no credential for their provider.`,
            `${allModels.length - readyModels.length} 个模型因缺凭证已隐藏。`,
          )}
        </span>
      )}
    </>
  );
}
