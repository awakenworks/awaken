// Provider-neutral inference and fallback routing controls. These values are
// authored with the Agent and frozen into its published revision; they are not
// provider credentials and never belong on the model-connection page.

import type { AgentConfig, InferenceOptions, ModelCandidate } from "../../lib/api/types";
import { Button, Card, TextField } from "../ui";
import { useApp } from "../../lib/app-state";

const EFFORTS: NonNullable<InferenceOptions["effort"]>[] = ["low", "medium", "high", "xhigh", "max"];
const SPEEDS: NonNullable<InferenceOptions["speed"]>[] = ["standard", "fast"];
const GEOGRAPHIES: NonNullable<InferenceOptions["inference_geo"]>[] = ["us", "eu", "apac", "cn", "jp", "au", "ca", "uk", "hk"];

export default function AgentModelRuntimeControls({
  config,
  onPatch,
  acp = false,
}: {
  config: AgentConfig;
  onPatch: (patch: Partial<AgentConfig>) => void;
  acp?: boolean;
}) {
  const app = useApp();
  const inference = config.inference ?? {};
  const candidates = config.model_candidates ?? [];
  const setInference = <K extends keyof InferenceOptions>(key: K, value: InferenceOptions[K] | "") => {
    const next = { ...inference };
    if (value === "") delete next[key];
    else next[key] = value;
    onPatch({ inference: next });
  };
  const setCandidate = (index: number, patch: Partial<ModelCandidate>) => onPatch({
    model_candidates: candidates.map((candidate, current) => current === index ? { ...candidate, ...patch } : candidate),
  });

  return (
    <Card style={{ marginTop: 14 }}>
      <h2 className="section-title">{app.t("Model execution policy", "模型执行策略")}</h2>
      <p className="hint">{app.t(
        acp
          ? "The selected ACP Harness owns model reasoning and session options. Awaken still owns explicit fallback routes."
          : "Choose reasoning, speed, processing geography, and exact fallback routes. These settings are reviewed and frozen with the Agent publication.",
        acp
          ? "所选 ACP Harness 负责模型推理与 Session 选项；Awaken 仍负责明确的备用路由。"
          : "配置推理强度、速度、处理地域和精确备用路由；这些设置会随 Agent 发布版本一起审阅并冻结。",
      )}</p>
      {acp ? (
        <div className="banner info">
          <span>ⓘ</span>
          <span>{app.t(
            "Reasoning, model, and Harness-specific options are shown under the selected ACP runtime above. Native inference settings remain in the draft but are not presented as active ACP controls.",
            "推理、模型及 Harness 专属选项显示在上方所选 ACP Runtime 下；草稿中的 Native 推理设置会保留，但不会被显示为生效的 ACP 配置。",
          )}</span>
        </div>
      ) : <div className="grid-3">
        <label className="field">
          <span>{app.t("Reasoning effort", "推理强度")}</span>
          <select className="input" value={inference.effort ?? ""} onChange={(event) => setInference("effort", event.target.value as InferenceOptions["effort"] | "")}>
            <option value="">{app.t("Model default", "模型默认")}</option>
            {EFFORTS.map((value) => <option value={value} key={value}>{value}</option>)}
          </select>
        </label>
        <label className="field">
          <span>{app.t("Inference speed", "推理速度")}</span>
          <select className="input" value={inference.speed ?? ""} onChange={(event) => setInference("speed", event.target.value as InferenceOptions["speed"] | "")}>
            <option value="">{app.t("Model default", "模型默认")}</option>
            {SPEEDS.map((value) => <option value={value} key={value}>{value}</option>)}
          </select>
        </label>
        <label className="field">
          <span>{app.t("Processing geography", "推理处理地域")}</span>
          <select className="input" value={inference.inference_geo ?? ""} onChange={(event) => setInference("inference_geo", event.target.value as InferenceOptions["inference_geo"] | "")}>
            <option value="">{app.t("No extra constraint", "不额外限制")}</option>
            {GEOGRAPHIES.map((value) => <option value={value} key={value}>{value.toUpperCase()}</option>)}
          </select>
        </label>
      </div>}

      <div className="field" style={{ marginTop: 16 }}>
        <label>{app.t("Fallback model routes", "备用模型路由")}</label>
        <span className="mut">{app.t(
          "Tried in order only after the primary route fails cleanly. Every identity is explicit; Awaken never guesses a provider account.",
          "仅在主路由明确失败后按顺序尝试。每个身份都必须明确，Awaken 不会猜测供应商账号。",
        )}</span>
        {candidates.map((candidate, index) => (
          <div className="agent-integration-row" key={index}>
            <TextField label={app.t("Provider identity", "供应商身份")} mono value={candidate.provider_identity_ref} onChange={(event) => setCandidate(index, { provider_identity_ref: event.target.value })} />
            <TextField label={app.t("Model", "模型")} mono value={candidate.model_ref} onChange={(event) => setCandidate(index, { model_ref: event.target.value })} />
            <TextField label={app.t("Backend", "执行后端")} mono value={candidate.backend_ref} onChange={(event) => setCandidate(index, { backend_ref: event.target.value })} />
            <Button variant="ghost" aria-label={app.t(`Remove fallback ${index + 1}`, `移除备用路由 ${index + 1}`)} onClick={() => onPatch({ model_candidates: candidates.filter((_, current) => current !== index) })}>✕</Button>
          </div>
        ))}
        <Button onClick={() => onPatch({
          model_candidates: [...candidates, { provider_identity_ref: "", model_ref: "", backend_ref: "genai" }],
        })}>+ {app.t("fallback route", "备用路由")}</Button>
      </div>
    </Card>
  );
}
