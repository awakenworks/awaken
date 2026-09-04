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
import { useEffect, useId, useRef } from "react";
import RuntimeCapabilitySummary from "./RuntimeCapabilitySummary";
import { acpWorkingDirectoryIssue } from "../../lib/agent-runtime-capabilities";

interface Props {
  model: AgentConfig["model"];
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  onChange: (model: AgentConfig["model"]) => void;
  onManage: () => void;
  onRefreshRuntimes?: () => void;
  runtimeUpdatedAt?: number;
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
  onRefreshRuntimes,
  runtimeUpdatedAt,
}: Props) {
  const app = useApp();
  const runtimeSelectId = useId();
  const modelSelectId = useId();
  const workingDirectoryId = useId();
  const workingDirectoryHelpId = useId();
  const workingDirectoryErrorId = useId();
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
  const workingDirectory = selected?.configuration?.working_directory ?? "";
  const workingDirectoryIssue = acpWorkingDirectoryIssue(workingDirectory);
  const rememberedSelections = useRef(new Map<string, AgentConfig["model"]>());
  useEffect(() => {
    rememberedSelections.current.set(runtimeId, model);
  }, [model, runtimeId]);
  const setConfiguration = (
    configuration: NonNullable<
      Extract<ModelSelection, { mode: "backend_default" | "backend_exact" }>["configuration"]
    >,
  ) => {
    if (selected) onChange({ ...selected, configuration });
  };

  return (
    <div className="runtime-selection-editor">
      <div className="field runtime-selection-field">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <label htmlFor={runtimeSelectId}>{app.t("Execution runtime", "执行 Runtime")}</label>
        {runtimeId === "awaken" ? (
          <button type="button" className="manage-link" onClick={onManage}>
            {app.t("Manage models ↗", "管理模型 ↗")}
          </button>
        ) : (
          <Link className="manage-link" to={protocolHelpPath(app.workspaceId, "acp")}>
            {app.t("ACP setup ↗", "ACP 设置 ↗")}
          </Link>
        )}
      </div>
      <select
        id={runtimeSelectId}
        className="input"
        aria-label={app.t("Execution runtime", "执行 Runtime")}
        value={runtimeId}
        onChange={(event) => {
          rememberedSelections.current.set(runtimeId, model);
          const target = event.target.value;
          const remembered = rememberedSelections.current.get(target);
          if (remembered) {
            onChange(remembered);
            return;
          }
          if (target === "awaken") {
            onChange({ mode: "auto" });
            return;
          }
          const next = runtimes.find((candidate) => candidate.id === target);
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
      </div>

      <div className="field runtime-selection-field">
      <label htmlFor={modelSelectId}>
        {runtimeId === "awaken" ? app.t("Model", "模型") : app.t("Harness model", "Harness 模型")}
      </label>
      {runtimeId === "awaken" && (providerModels.length > 0) ? (
        <select
          id={modelSelectId}
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
          id={modelSelectId}
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
      </div>
      {selected && runtime?.local?.negotiated && (
        <div className="row" style={{ marginTop: 10, alignItems: "flex-start" }}>
          {runtime.local.negotiated.modes.length > 0 && (
            <div className="field" style={{ flex: 1 }}>
              <label>{app.t("ACP mode", "ACP 模式")}</label>
              <select
                className="input mono"
                aria-label={app.t("ACP mode", "ACP 模式")}
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
                  aria-label={option.name}
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
      {selected && (
        <div className="field acp-working-directory" style={{ marginTop: 10 }}>
          <div className="field-label-row">
            <label htmlFor={workingDirectoryId}>{app.t("ACP working directory", "ACP 工作目录")}</label>
            <span className="field-optional">{app.t("Optional", "可选")}</span>
          </div>
          <div className="input-with-action">
            <input
              id={workingDirectoryId}
              className="input mono"
              aria-label={app.t("ACP working directory", "ACP 工作目录")}
              placeholder="repo/subdirectory"
              value={workingDirectory}
              aria-invalid={workingDirectoryIssue !== null}
              aria-describedby={workingDirectoryIssue ? `${workingDirectoryHelpId} ${workingDirectoryErrorId}` : workingDirectoryHelpId}
              onChange={(event) => {
                const workingDirectory = event.target.value;
                setConfiguration({
                  ...(selected.configuration ?? {}),
                  working_directory: workingDirectory || null,
                });
              }}
            />
            {workingDirectory && (
              <button
                type="button"
                className="btn ghost compact"
                onClick={() => setConfiguration({ ...(selected.configuration ?? {}), working_directory: null })}
              >{app.t("Use workspace root", "使用工作区根目录")}</button>
            )}
          </div>
          <span id={workingDirectoryHelpId} className="mut" style={{ fontSize: 12 }}>
            {workingDirectory
              ? app.t(`Resolved inside Session workspace: /${workingDirectory}`, `Session 工作区内的解析位置：/${workingDirectory}`)
              : app.t("The Harness starts at the Session workspace root.", "Harness 将从 Session 工作区根目录启动。")}
          </span>
          {workingDirectoryIssue && (
            <div id={workingDirectoryErrorId} className="banner warn" role="alert">
              <span>!</span>
              <span>{workingDirectoryIssue === "too_long"
                ? app.t("Keep the working directory at 512 characters or fewer.", "工作目录不能超过 512 个字符。")
                : workingDirectoryIssue === "absolute"
                  ? app.t("Use a path relative to the Session workspace, such as repo/src.", "请使用相对于 Session 工作区的路径，例如 repo/src。")
                  : workingDirectoryIssue === "traversal"
                    ? app.t("Remove . or .. segments. The working directory cannot leave its Session workspace.", "请移除 . 或 .. 路径段；工作目录不能离开 Session 工作区。")
                    : workingDirectoryIssue === "backslash"
                      ? app.t("Use forward slashes, for example repo/src.", "请使用正斜杠，例如 repo/src。")
                      : workingDirectoryIssue === "empty_segment"
                        ? app.t("Remove repeated or trailing slashes.", "请移除重复或末尾的斜杠。")
                        : app.t("Colons are not allowed in a portable Session working directory.", "可移植的 Session 工作目录中不允许使用冒号。")}</span>
            </div>
          )}
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
      <RuntimeCapabilitySummary
        model={model}
        runtime={selectedRuntime}
        updatedAt={runtimeUpdatedAt}
        onRefresh={onRefreshRuntimes}
      />
    </div>
  );
}
