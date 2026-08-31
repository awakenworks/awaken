import type { AgentConfig, ModelSelection, RuntimeCap } from "../../lib/api/types";
import {
  acpModelChoices,
  defaultSelectionForRuntime,
  executionRuntimeId,
} from "../../lib/agent-model-selection";
import { useApp } from "../../lib/app-state";
import { protocolHelpPath } from "../../lib/navigation/paths";
import { runtimeStatus } from "../../lib/readiness";
import { Link } from "react-router";
import RuntimeCapabilitySummary from "./RuntimeCapabilitySummary";

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

function harnessModelValue(model: AgentConfig["model"]): string {
  if (typeof model !== "object" || !("mode" in model)
    || (model.mode !== "backend_default" && model.mode !== "backend_exact")) return "";
  return JSON.stringify(model.mode === "backend_default"
    ? { mode: model.mode, backend_ref: model.backend_ref }
    : { mode: model.mode, backend_ref: model.backend_ref, model_ref: model.model_ref });
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
  const runtimeId = executionRuntimeId(model);
  const selectedRuntime = runtimes.find((candidate) => candidate.id === runtimeId);
  const choices = acpModelChoices(selectedRuntime ? [selectedRuntime] : []);
  const selected =
    typeof model === "object" && "mode" in model
      && (model.mode === "backend_default" || model.mode === "backend_exact")
      ? model
      : null;
  const runtime = selected
    ? runtimes.find((candidate) => candidate.id === selected.backend_ref)
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
        <span>{app.t("Execution runtime", "执行 Runtime")}</span>
        {runtimeId === "awaken" ? (
          <button className="manage-link" onClick={onManage}>
            {app.t("Manage models ↗", "管理模型 ↗")}
          </button>
        ) : (
          <Link className="manage-link" to={protocolHelpPath(app.workspaceId, "acp")}>
            {app.t("ACP setup ↗", "ACP 设置 ↗")}
          </Link>
        )}
      </label>
      <select
        className="input"
        aria-label={app.t("Execution runtime", "执行 Runtime")}
        value={runtimeId}
        onChange={(event) => {
          if (event.target.value === "awaken") {
            onChange({ mode: "auto" });
            return;
          }
          const next = runtimes.find((candidate) => candidate.id === event.target.value);
          const selection = next && defaultSelectionForRuntime(next);
          if (selection) onChange(selection);
        }}
      >
        <option value="awaken">{app.t("Native Awaken", "Native Awaken")}</option>
        {runtimes.filter((candidate) => candidate.kind === "acp").map((candidate) => {
          const status = runtimeStatus(candidate);
          const suffix = status === "ready"
            ? app.t("ready", "就绪")
            : status === "login_required"
              ? app.t("login required", "需要登录")
              : app.t("not detected", "未检测到");
          return (
            <option key={candidate.id} value={candidate.id} disabled={status !== "ready"}>
              {candidate.label} · ACP · {suffix}
            </option>
          );
        })}
      </select>
      <span className="mut" style={{ fontSize: 12 }}>
        {runtimeId === "awaken"
          ? app.t(
              "Awaken runs the model loop directly. Environment and governance stay the same when you switch Harnesses.",
              "Awaken 直接运行模型循环；切换 Harness 时，Environment 与治理能力保持不变。",
            )
          : app.t(
              "The selected ACP Harness runs the model loop inside the same Awaken Session and Environment.",
              "所选 ACP Harness 在同一个 Awaken Session 与 Environment 中运行模型循环。",
            )}
      </span>

      <label style={{ marginTop: 12 }}>
        {runtimeId === "awaken" ? app.t("Model", "模型") : app.t("Harness model", "Harness 模型")}
      </label>
      {runtimeId === "awaken" && (providerModels.length > 0) ? (
        <select
          className="input mono"
          aria-label={app.t("Model", "模型")}
          value={selectionValue(model)}
          onChange={(event) => onChange(JSON.parse(event.target.value) as AgentConfig["model"])}
        >
          <option value="">{app.t("— select a model —", "— 选择模型 —")}</option>
          <option value={selectionValue({ mode: "auto" })}>
            {app.t("Automatically choose a ready model", "自动选择可用模型")}
          </option>
          {providerModels.map((id) => (
            <option key={`provider:${id}`} value={selectionValue({ id })}>
              {id}{!readyModels.includes(id) ? app.t("  ⚠ no credential", "  ⚠ 无凭证") : ""}
            </option>
          ))}
        </select>
      ) : runtimeId !== "awaken" && choices.length > 0 ? (
        <select
          className="input mono"
          aria-label={app.t("Harness model", "Harness 模型")}
          value={harnessModelValue(model)}
          onChange={(event) => {
            const next = JSON.parse(event.target.value) as Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>;
            onChange(selected?.configuration ? { ...next, configuration: selected.configuration } : next);
          }}
        >
          {choices.map((choice) => (
            <option key={choice.key} value={harnessModelValue(choice.selection)}>{choice.label}</option>
          ))}
        </select>
      ) : (
        <div className="banner gate">
          <span>◌</span>
          <span>{app.t(
            runtimeId === "awaken"
              ? "No runnable model is available. Open Models, connect a provider, and verify a real response before continuing."
              : "This ACP Harness is not ready. Install it, sign in, and refresh its capability probe.",
            runtimeId === "awaken"
              ? "没有可运行模型。请打开“模型”，连接供应商并验证一次真实响应后继续。"
              : "该 ACP Harness 尚未就绪。请完成安装和登录，再刷新能力探测。",
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
      <RuntimeCapabilitySummary model={model} runtime={selectedRuntime} />
    </>
  );
}
