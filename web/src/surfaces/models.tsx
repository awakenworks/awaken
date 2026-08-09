// Workspace Models: Provider → ProtocolEndpoint → Offering, profiles and dry-run resolve.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useEffect, useRef, useState } from "react";
import Transcript from "../components/session/Transcript";
import { Button, Card, Modal, Pill, Skeleton, UsageBadges } from "../components/ui";
import { api, workspaceQuery, ws } from "../lib/api/client";
import type {
  CatalogSyncResult,
  CredentialSource,
  ProviderCatalog,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
  Session,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import {
  cloudModelUiState,
  CloudModelBadge,
  CloudModelNotice,
  CloudModelRefresh,
  modelCatalogPresentation,
} from "./model-cloud-capability";
import ProviderConnectionPanel from "./provider-connection-panel";

/** Compact a token count for the catalog list: 200000 → "200k", 1_000_000 → "1M". */
function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n % 1_000_000 ? 1 : 0)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}k`;
  return String(n);
}

function fmtObservedAt(timestamp: number): string {
  return new Date(timestamp).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** A live model test: a scratch session (via the `default` agent) pinned to the
 * chosen model, so a real reply proves the connection — the same transcript
 * engine as the Sandbox. No provider key → the run errors honestly, not a stub. */
function TestChat({ model }: { model: string }) {
  const app = useApp();
  const [sid, setSid] = useState<string | null>(null);
  const [latencyMs, setLatencyMs] = useState<number | undefined>();
  const started = useRef(false);
  const start = useMutation({
    mutationFn: () =>
      api.post<Session>(ws("/v1/sessions"), { agent: "default", title: `test · ${model}` }),
    onSuccess: (s) => setSid(s.id),
  });
  useEffect(() => {
    if (!started.current) {
      started.current = true;
      start.mutate();
    }
  }, [start]);
  const session = useQuery({
    queryKey: ["test-session", sid],
    enabled: !!sid,
    queryFn: () => api.get<Session>(ws(`/v1/sessions/${sid}`)),
    refetchInterval: 4_000,
  });
  if (start.error instanceof Error) {
    return (
      <div className="banner gate">
        <span>!</span>
        <span style={{ flex: 1 }}>
          <strong>{app.t("Connection test could not start", "连接测试无法启动")}</strong>
          <small>{start.error.message}</small>
        </span>
        <Button
          disabled={start.isPending}
          onClick={() => {
            start.reset();
            start.mutate();
          }}
        >
          {app.t("Retry", "重试")}
        </Button>
      </div>
    );
  }
  if (!sid) {
    return (
      <div>
        <Skeleton height={60} />
        <p className="hint">{app.t("Starting a secure test session…", "正在启动安全测试会话…")}</p>
      </div>
    );
  }
  return (
    <>
      <div className="row" style={{ margin: "4px 0 8px" }}>
        <Pill tone="agent">{model}</Pill>
        <UsageBadges usage={session.data?.usage} latencyMs={latencyMs} />
      </div>
      <Transcript
        base={ws(`/v1/sessions/${sid}`)}
        queryKey={["test-events", sid]}
        fixedModel={model}
        autoMessage={{
          id: `model-test-${sid}`,
          text: "Reply with a short confirmation that this model connection is working.",
        }}
        placeholder={app.t("Say hello…", "打个招呼…")}
        onLatency={setLatencyMs}
      />
    </>
  );
}

export default function ModelsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["catalog", workspace],
    queryFn: () => api.get<ProviderCatalog>(ws("/v1/config/catalog")),
  });
  const capabilities = useConfigCapabilities();
  const descriptors = useQuery({
    queryKey: ["provider-descriptors", workspace],
    queryFn: () => api.get<ProviderDriverDescriptor[]>(ws("/v1/config/provider-descriptors")),
    staleTime: Infinity,
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const connections = useQuery({
    queryKey: ["provider-connections", workspace],
    queryFn: () =>
      api.get<ProviderConnectionSummary[]>(
        ws(workspaceQuery("/v1/config/provider-connections", workspace)),
      ),
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(workspaceQuery("/v1/config/credentials", workspace))),
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const [testModel, setTestModel] = useState<string | null>(null);
  const refreshCloudModels = useMutation({
    mutationFn: () =>
      api.post<CatalogSyncResult>(ws("/v1/config/brokered-models/refresh")),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["catalog", workspace] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const c = catalog.data;
  const cloudState = cloudModelUiState(capabilities.data);
  const managedSupply = cloudState === "managed";
  const presentation = modelCatalogPresentation(cloudState);
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const offeringsByProvider = new Map<string, ProviderCatalog["offerings"]>();
  for (const offering of c?.offerings ?? []) {
    const group = offeringsByProvider.get(offering.provider_id) ?? [];
    group.push(offering);
    offeringsByProvider.set(offering.provider_id, group);
  }
  return (
    <>
      <Card style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px", justifyContent: "space-between" }}>
          <div className="row">
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Catalog", "模型目录")}</h2>
          <CloudModelBadge state={cloudState} />
          <span className="mut">
            {c
              ? managedSupply
                ? `${Object.keys(c.providers).length} providers · ${c.offerings.length} models`
                : `${Object.keys(c.providers).length} providers · ${Object.keys(c.endpoints).length} endpoints · ${c.offerings.length} offerings`
              : "…"}
          </span>
          </div>
          <CloudModelRefresh
            state={cloudState}
            pending={refreshCloudModels.isPending}
            onRefresh={() => refreshCloudModels.mutate()}
          />
        </div>
        <CloudModelNotice state={cloudState} />
        {refreshCloudModels.error instanceof Error && (
          <div className="err" style={{ margin: "0 16px 12px" }}>
            {refreshCloudModels.error.message}
          </div>
        )}
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              {presentation.showSupplyInfrastructure && <th>Provider</th>}
              {presentation.showSupplyInfrastructure && <th>Endpoint</th>}
              {presentation.showSupplyInfrastructure && <th>Dialect</th>}
              <th>Status</th>
              <th>{app.t("Context", "上下文")}</th>
              <th>{app.t("Max output", "最大输出")}</th>
              {presentation.allowSessionTest && <th style={{ textAlign: "right" }}></th>}
            </tr>
          </thead>
          <tbody>
            {[...offeringsByProvider.entries()]
              .sort(([left], [right]) => left.localeCompare(right))
              .map(([providerId, offerings]) => (
              <Fragment key={providerId}>
                <tr>
                  <td colSpan={managedSupply ? 4 : 8} style={{ fontWeight: 650, background: "var(--surface-subtle)" }}>
                    {c?.providers?.[providerId]?.display_name ?? providerId}
                    {!managedSupply && <span className="mut" style={{ marginLeft: 8 }}>{providerId}</span>}
                  </td>
                </tr>
                {offerings.map((o, i) => (
              <tr key={`${o.model_id}-${o.protocol_endpoint_id}-${i}`}>
                <td className="mono">{o.model_id}</td>
                {presentation.showSupplyInfrastructure && <td>{o.provider_id}</td>}
                {presentation.showSupplyInfrastructure && <td className="mono mut">{o.protocol_endpoint_id}</td>}
                {presentation.showSupplyInfrastructure && <td>
                  <Pill tone="neutral">{o.dialect}</Pill>
                </td>}
                <td>
                  <Pill tone={(o.status ?? "active") === "active" ? "agent" : "neutral"}>
                    {o.status ?? "active"}{managedSupply ? "" : ` · ${o.source ?? "manual"}`}
                  </Pill>
                  {!managedSupply && (o.source ?? "manual") === "provider_api" && (
                    <div
                      className="mut"
                      title={
                        o.last_seen_at_unix_ms
                          ? new Date(o.last_seen_at_unix_ms).toISOString()
                          : app.t("No successful observation recorded", "尚无成功发现记录")
                      }
                      style={{ marginTop: 4, fontSize: 11 }}
                    >
                      {o.last_seen_at_unix_ms
                        ? `${app.t("last seen", "上次发现")} ${fmtObservedAt(o.last_seen_at_unix_ms)}`
                        : app.t("last seen unknown", "上次发现时间未知")}
                    </div>
                  )}
                </td>
                <td className="mut">
                  {c?.model_attributes?.[o.model_id]?.context_window
                    ? `${fmtTokens(c.model_attributes[o.model_id].context_window!)}`
                    : "—"}
                  {c?.model_attributes?.[o.model_id]?.provenance?.context_window?.source && (
                    <div style={{ fontSize: 11 }}>
                      {c.model_attributes[o.model_id].provenance!.context_window.source}
                    </div>
                  )}
                </td>
                <td className="mut">
                  {c?.model_attributes?.[o.model_id]?.max_output_tokens
                    ? `${fmtTokens(c.model_attributes[o.model_id].max_output_tokens!)}`
                    : "—"}
                  {c?.model_attributes?.[o.model_id]?.provenance?.max_output_tokens?.source && (
                    <div style={{ fontSize: 11 }}>
                      {c.model_attributes[o.model_id].provenance!.max_output_tokens.source}
                    </div>
                  )}
                </td>
                {presentation.allowSessionTest && <td style={{ textAlign: "right" }}>
                  <Button
                    style={{ height: 24 }}
                    disabled={(o.status ?? "active") !== "active"}
                    onClick={() => setTestModel(o.model_id)}
                  >
                    {app.t("Test", "测试")}
                  </Button>
                </td>}
              </tr>
                ))}
              </Fragment>
            ))}
            {(c?.offerings ?? []).length === 0 && (
              <tr>
                <td colSpan={managedSupply ? 4 : 8} className="mut">
                  {app.t(
                    byokEnabled
                      ? "No models yet — connect a provider below."
                      : "No managed models are currently available. Contact your workspace administrator.",
                    byokEnabled
                      ? "暂无模型——请在下方连接供应商。"
                      : "当前没有可用的托管模型，请联系 Workspace 管理员。",
                  )}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>

      {byokEnabled && (
        <ProviderConnectionPanel
          credentials={credentials.data ?? []}
          descriptors={descriptors.data ?? []}
          connections={connections.data ?? []}
        />
      )}

      {presentation.allowSessionTest && testModel && (
        <Modal
          title={app.t(`Test model · ${testModel}`, `测试模型 · ${testModel}`)}
          onClose={() => setTestModel(null)}
          width="min(680px, 94vw)"
        >
          <p className="hint">
            {app.t(
              "A live round-trip through the resolve chain — a real reply proves the connection.",
              "一次经过 resolve 链的真实往返——真实回复即证明连接可用。",
            )}
          </p>
          <TestChat model={testModel} />
        </Modal>
      )}
    </>
  );
}
