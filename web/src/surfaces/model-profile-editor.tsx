import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Button, Card, Pill, SelectField } from "../components/ui";
import { api, isAbsent, ws } from "../lib/api/client";
import type {
  CredentialBinding,
  CredentialSource,
  InferenceProfile,
  ModelTarget,
  Offering,
  ProfileCandidate,
  ResolvedCandidatesView,
  ResolvedInferenceView,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

export interface ProfileDraftCandidate {
  targetKey: string;
  credentialId: string;
}

export function offeringTarget(offering: Offering): ModelTarget {
  return {
    model_id: offering.model_id,
    provider_id: offering.provider_id,
    protocol_endpoint_id: offering.protocol_endpoint_id,
  };
}

export function targetKey(target: ModelTarget): string {
  return JSON.stringify([
    target.model_id,
    target.provider_id ?? "",
    target.protocol_endpoint_id ?? "",
  ]);
}

export function targetFromKey(key: string): ModelTarget {
  const [model_id, provider_id, protocol_endpoint_id] = JSON.parse(key) as string[];
  return { model_id, provider_id, protocol_endpoint_id };
}

export function profileCandidateOf(draft: ProfileDraftCandidate): ProfileCandidate {
  const credential_binding: CredentialBinding = draft.credentialId
    ? { type: "exact", credential_source_id: draft.credentialId }
    : { type: "none" };
  return { target: targetFromKey(draft.targetKey), credential_binding };
}

export function appendFallback(
  current: ProfileDraftCandidate[],
  target: string,
  primary: string,
): ProfileDraftCandidate[] {
  if (
    !target ||
    target === primary ||
    current.some((item) => item.targetKey === target) ||
    current.length >= 8
  ) {
    return current;
  }
  return [...current, { targetKey: target, credentialId: "" }];
}

export function moveFallback(
  current: ProfileDraftCandidate[],
  index: number,
  delta: -1 | 1,
): ProfileDraftCandidate[] {
  const destination = index + delta;
  if (index < 0 || index >= current.length || destination < 0 || destination >= current.length) {
    return current;
  }
  const next = [...current];
  [next[index], next[destination]] = [next[destination], next[index]];
  return next;
}

function ResolveChain({ view }: { view: ResolvedInferenceView }) {
  return (
    <div className="chain" style={{ marginTop: 10 }}>
      <span className="chip mono">{view.model_id}</span>
      <span className="arrow">→</span>
      <span className="chip">
        credential
        <span className="dot" style={{ background: view.credential_present ? "var(--ok)" : "var(--fg3)" }} />
        {view.credential_present ? "present" : "none"}
      </span>
      <span className="arrow">→</span>
      <span className="chip">
        {view.provider_id}
        <span className="mono mut">{view.adapter_kind}</span>
        <span className="dot" style={{ background: "var(--ok)" }} />
      </span>
      {view.base_url && <code className="mut">{view.base_url}</code>}
    </div>
  );
}

export default function ModelProfileEditor({
  offerings,
  credentials,
}: {
  offerings: Offering[];
  credentials: CredentialSource[];
}) {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const profileId = "workspace-default";
  const offeringOptions = offerings.filter((offering) => (offering.status ?? "active") === "active");
  const [primaryTargetKey, setPrimaryTargetKey] = useState("");
  const [primaryCredentialId, setPrimaryCredentialId] = useState("");
  const [profileFallbacks, setProfileFallbacks] = useState<ProfileDraftCandidate[]>([]);
  const profileHydrated = useRef<string | null>(null);
  const profile = useQuery({
    queryKey: ["inference-profile", workspace, profileId],
    queryFn: async () => {
      try {
        return await api.get<InferenceProfile>(ws(`/v1/config/inference-profiles/${profileId}`));
      } catch (error) {
        if (isAbsent(error)) return null;
        throw error;
      }
    },
    retry: false,
  });
  useEffect(() => {
    if (profileHydrated.current === workspace || profile.isLoading) return;
    if (profile.data) {
      setPrimaryTargetKey(targetKey(profile.data.primary.target));
      setPrimaryCredentialId(
        profile.data.primary.credential_binding.type === "exact"
          ? profile.data.primary.credential_binding.credential_source_id
          : "",
      );
      setProfileFallbacks(
        profile.data.fallbacks.map((candidate) => ({
          targetKey: targetKey(candidate.target),
          credentialId:
            candidate.credential_binding.type === "exact"
              ? candidate.credential_binding.credential_source_id
              : "",
        })),
      );
      profileHydrated.current = workspace;
    } else if (offeringOptions[0]) {
      setPrimaryTargetKey(targetKey(offeringTarget(offeringOptions[0])));
      profileHydrated.current = workspace;
    }
  }, [offeringOptions, profile.data, profile.isLoading, workspace]);
  const saveProfile = useMutation({
    mutationFn: () => {
      if (!primaryTargetKey) throw new Error("Choose a primary model");
      return api.put<InferenceProfile>(ws(`/v1/config/inference-profiles/${profileId}`), {
        workspace_id: workspace,
        primary: profileCandidateOf({ targetKey: primaryTargetKey, credentialId: primaryCredentialId }),
        fallbacks: profileFallbacks.map(profileCandidateOf),
        disabled_endpoint_ids: [],
      });
    },
    onSuccess: (saved) => qc.setQueryData(["inference-profile", workspace, profileId], saved),
  });
  const previewProfile = useMutation({
    mutationFn: () =>
      api.post<ResolvedCandidatesView>(
        ws(`/v1/config/inference-profiles/${profileId}/resolve-candidates`),
        { workspace_id: workspace },
      ),
  });
  const offeringEntries = offeringOptions.map((offering) => ({
    offering,
    key: targetKey(offeringTarget(offering)),
  }));
  const firstUnusedFallback = offeringEntries.find(
    ({ key }) =>
      key !== primaryTargetKey && !profileFallbacks.some((candidate) => candidate.targetKey === key),
  )?.key;
  const credentialsFor = (key: string) => {
    const provider = offeringEntries.find((entry) => entry.key === key)?.offering.provider_id;
    return credentials.filter(
      (credential) =>
        credential.status === "active" &&
        (credential.provider_id == null || credential.provider_id === provider),
    );
  };
  const updateFallback = (index: number, patch: Partial<ProfileDraftCandidate>) =>
    setProfileFallbacks((current) =>
      current.map((candidate, candidateIndex) =>
        candidateIndex === index ? { ...candidate, ...patch } : candidate,
      ),
    );

  return (
    <Card>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <div>
          <h2>{app.t("Workspace inference profile", "工作区推理配置")}</h2>
          <p className="hint">
            {app.t(
              "Choose one exact primary offering and explicit ordered fallbacks. Each step owns its credential; Awaken never inserts an implicit Cloud or local fallback.",
              "选择一个精确的主模型，并显式排列 fallback。每一步独立绑定凭证；Awaken 不会暗中添加云端或本地 fallback。",
            )}
          </p>
        </div>
        <Pill tone="neutral">{profileId}</Pill>
      </div>
      <div className="row" style={{ alignItems: "flex-end" }}>
        <SelectField label={app.t("Primary model", "主模型")} value={primaryTargetKey} onChange={(event) => {
          const next = event.target.value;
          setPrimaryTargetKey(next);
          setPrimaryCredentialId("");
          setProfileFallbacks((current) => current.filter((candidate) => candidate.targetKey !== next));
        }}>
          <option value="">{app.t("Choose an offering", "选择模型 Offering")}</option>
          {offeringEntries.map(({ offering, key }) => <option key={key} value={key}>{offering.model_id} · {offering.provider_id} · {offering.protocol_endpoint_id} · {offering.source ?? "manual"}</option>)}
        </SelectField>
        <SelectField label={app.t("Primary credential", "主模型凭证")} value={primaryCredentialId} onChange={(event) => setPrimaryCredentialId(event.target.value)}>
          <option value="">{app.t("Managed / no BYOK credential", "云端托管 / 不使用 BYOK 凭证")}</option>
          {credentialsFor(primaryTargetKey).map((credential) => <option key={credential.id} value={credential.id}>{credential.id} · {credential.provider_id ?? "unscoped"}</option>)}
        </SelectField>
      </div>
      {profileFallbacks.map((candidate, index) => (
        <div className="row" key={`${candidate.targetKey}-${index}`} style={{ marginTop: 10, alignItems: "flex-end" }}>
          <Pill tone="neutral">Fallback {index + 1}</Pill>
          <SelectField label={app.t("Exact model offering", "精确模型 Offering")} value={candidate.targetKey} onChange={(event) => updateFallback(index, { targetKey: event.target.value, credentialId: "" })}>
            {offeringEntries.map(({ offering, key }) => <option key={key} value={key} disabled={key === primaryTargetKey || profileFallbacks.some((other, otherIndex) => otherIndex !== index && other.targetKey === key)}>{offering.model_id} · {offering.provider_id} · {offering.protocol_endpoint_id} · {offering.source ?? "manual"}</option>)}
          </SelectField>
          <SelectField label={app.t("Credential for this step", "本步骤凭证")} value={candidate.credentialId} onChange={(event) => updateFallback(index, { credentialId: event.target.value })}>
            <option value="">{app.t("Managed / no BYOK credential", "云端托管 / 不使用 BYOK 凭证")}</option>
            {credentialsFor(candidate.targetKey).map((credential) => <option key={credential.id} value={credential.id}>{credential.id} · {credential.provider_id ?? "unscoped"}</option>)}
          </SelectField>
          <Button disabled={index === 0} onClick={() => setProfileFallbacks((current) => moveFallback(current, index, -1))}>↑</Button>
          <Button disabled={index === profileFallbacks.length - 1} onClick={() => setProfileFallbacks((current) => moveFallback(current, index, 1))}>↓</Button>
          <Button onClick={() => setProfileFallbacks((current) => current.filter((_, itemIndex) => itemIndex !== index))}>{app.t("Remove", "移除")}</Button>
        </div>
      ))}
      <div className="row" style={{ marginTop: 12 }}>
        <Button disabled={!firstUnusedFallback || profileFallbacks.length >= 8} onClick={() => firstUnusedFallback && setProfileFallbacks((current) => appendFallback(current, firstUnusedFallback, primaryTargetKey))}>{app.t("Add fallback", "添加 fallback")}</Button>
        <Button variant="primary" disabled={!primaryTargetKey || saveProfile.isPending} onClick={() => saveProfile.mutate()}>{saveProfile.isPending ? app.t("Saving…", "保存中…") : app.t("Save profile", "保存配置")}</Button>
        <Button disabled={previewProfile.isPending || !profile.data} onClick={() => previewProfile.mutate()}>{app.t("Validate failover chain", "验证 fallback 链")}</Button>
        {saveProfile.isSuccess && <span className="mut">✓ {app.t("Saved", "已保存")}</span>}
      </div>
      {saveProfile.error instanceof Error && <div className="err">{saveProfile.error.message}</div>}
      {previewProfile.error instanceof Error && <div className="err">{previewProfile.error.message}</div>}
      {previewProfile.data?.candidates.map((candidate, index) => <div key={`${candidate.provider_id}-${candidate.protocol_endpoint_id}-${index}`}><span className="mut">{index === 0 ? app.t("Primary", "主模型") : `Fallback ${index}`}</span><ResolveChain view={candidate} /></div>)}
    </Card>
  );
}
