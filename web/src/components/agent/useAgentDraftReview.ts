import { useEffect, useRef, useState, type Dispatch, type SetStateAction } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api } from "../../lib/api/client";
import type { AgentConfig, AgentConfigItem, ValidationIssue, ValidationResult } from "../../lib/api/types";
import { diffConfig } from "../../lib/config-diff";
import {
  AGENT_DRAFT_CHANGED_EVENT,
  ASSISTANT_REPAIR_EVENT,
  ASSISTANT_SETTLED_EVENT,
} from "../../lib/assistant-events";

export type ReviewStatus = "idle" | "saving" | "validating" | "fixing" | "ready" | "needs_input";

interface DraftReviewOptions {
  agentId: string;
  config: AgentConfig;
  dirty: boolean;
  canSave: boolean;
  readyModels: number;
  blank: AgentConfig;
  setConfig: Dispatch<SetStateAction<AgentConfig>>;
  setDirty: Dispatch<SetStateAction<boolean>>;
  onIssues: (issues: ValidationIssue[]) => void;
  onOpenPublish: () => void;
  onError: (error: unknown) => void;
}

/** Coordinates the reversible Draft workflow. The config plane remains authoritative:
 * local edits are saved before compile, Assistant tool completion triggers a re-read,
 * and only a clean Draft can open the irreversible Publish confirmation. */
export function useAgentDraftReview(options: DraftReviewOptions) {
  const qc = useQueryClient();
  const [status, setStatus] = useState<ReviewStatus>("idle");
  const [changedPaths, setChangedPaths] = useState<string[]>([]);
  const configRef = useRef(options.config);
  const dirtyRef = useRef(options.dirty);
  const optionsRef = useRef(options);
  const repairInFlight = useRef(false);
  const draftRefreshInFlight = useRef(false);
  const repairAttempts = useRef(0);

  useEffect(() => {
    configRef.current = options.config;
    dirtyRef.current = options.dirty;
    optionsRef.current = options;
  }, [options]);

  const reset = () => {
    dirtyRef.current = false;
    repairInFlight.current = false;
    repairAttempts.current = 0;
    setChangedPaths([]);
    setStatus("idle");
  };

  const onManualEdit = (paths: string[]) => {
    dirtyRef.current = true;
    setStatus("idle");
    setChangedPaths((current) => current.filter((path) =>
      !paths.some((authored) => path === authored || path.startsWith(`${authored}.`))));
  };

  const markSaved = () => {
    dirtyRef.current = false;
  };

  const setValidationResult = (valid: boolean) => setStatus(valid ? "ready" : "needs_input");
  const markReady = () => setStatus("ready");

  const askAgentToRepair = (found: ValidationIssue[]) => {
    const current = optionsRef.current;
    repairInFlight.current = true;
    repairAttempts.current += 1;
    setStatus("fixing");
    const summary = found.map((issue) => `${issue.path || "config"}: ${issue.message}`).join("\n");
    window.dispatchEvent(new CustomEvent(ASSISTANT_REPAIR_EVENT, {
      detail: {
        id: current.agentId,
        requestId: `repair-${current.agentId}-${Date.now()}-${repairAttempts.current}`,
        message: [
          "Fix this saved agent Draft automatically. Keep the operator's intent, change only what is needed,",
          "call admin_patch_agent, then admin_validate_agent, and finish only when it validates. Do not publish.",
          "Validation issues:",
          summary,
        ].join("\n"),
      },
    }));
  };

  const preparePublish = async (publishPending: boolean) => {
    const current = optionsRef.current;
    if (!current.canSave || publishPending) return;
    const body = { ...configRef.current, id: current.agentId };
    try {
      if (dirtyRef.current) {
        setStatus("saving");
        await api.put(`/v1/config/agents/${current.agentId}`, body);
        current.setDirty(false);
        dirtyRef.current = false;
        void qc.invalidateQueries({ queryKey: ["config-agents"] });
      }
      setStatus("validating");
      const result = await api.post<ValidationResult>(`/v1/config/agents/${current.agentId}/validate`, body);
      current.onIssues(result.issues ?? []);
      if (result.valid) {
        setStatus("ready");
        current.onOpenPublish();
      } else if (current.readyModels > 0) {
        askAgentToRepair(result.issues ?? []);
      } else {
        setStatus("needs_input");
      }
    } catch (error) {
      setStatus("needs_input");
      current.onError(error);
    }
  };

  useEffect(() => {
    const onDraftChanged = (event: Event) => {
      const detail = (event as CustomEvent<{ id?: string; paths?: string[] }>).detail;
      const current = optionsRef.current;
      if (detail?.id !== current.agentId) return;
      draftRefreshInFlight.current = true;
      setStatus("validating");
      void (async () => {
        try {
          const item = await api.get<AgentConfigItem>(`/v1/config/agents/${detail.id}`);
          const { published: _published, ...rest } = item;
          const saved = { ...current.blank, ...rest };
          const hintedPaths = detail.paths ?? [];
          const next = structuredClone(configRef.current);
          const assignPath = (path: string) => {
            const parts = path.split(".");
            let target = next as unknown as Record<string, unknown>;
            let source = saved as unknown as Record<string, unknown>;
            for (let index = 0; index < parts.length - 1; index += 1) {
              const part = parts[index];
              const sourceValue = source[part];
              if (!sourceValue || typeof sourceValue !== "object" || Array.isArray(sourceValue)) return;
              source = sourceValue as Record<string, unknown>;
              const targetValue = target[part];
              if (!targetValue || typeof targetValue !== "object" || Array.isArray(targetValue)) target[part] = {};
              target = target[part] as Record<string, unknown>;
            }
            const leaf = parts.at(-1)!;
            if (source[leaf] === undefined) delete target[leaf];
            else target[leaf] = structuredClone(source[leaf]);
          };
          if (hintedPaths.length > 0) hintedPaths.filter((path) => path !== "resources").forEach(assignPath);
          else Object.assign(next, saved);
          if (hintedPaths.some((path) => path.startsWith("plugin_config."))) next.plugins = saved.plugins;
          const paths = hintedPaths.length > 0
            ? [...hintedPaths]
            : diffConfig(configRef.current, next).map((change) => change.path);
          const pluginIds = new Set([...configRef.current.plugins, ...next.plugins]);
          for (const pluginId of pluginIds) {
            if (configRef.current.plugins.includes(pluginId) !== next.plugins.includes(pluginId)) {
              paths.push(`plugin_config.${pluginId}`);
            }
          }
          setChangedPaths(Array.from(new Set(paths)));
          current.setConfig(next);
          configRef.current = next;
          current.setDirty(dirtyRef.current);
          if (hintedPaths.includes("resources")) {
            void qc.invalidateQueries({ queryKey: ["agent-resources", detail.id] });
          }
          const result = await api.post<ValidationResult>(`/v1/config/agents/${detail.id}/validate`, next);
          current.onIssues(result.issues ?? []);
          if (result.valid) {
            const repairing = repairInFlight.current;
            repairInFlight.current = false;
            draftRefreshInFlight.current = false;
            setStatus("ready");
            if (repairing) current.onOpenPublish();
          } else if (repairInFlight.current && repairAttempts.current < 3) {
            draftRefreshInFlight.current = false;
            askAgentToRepair(result.issues ?? []);
          } else {
            repairInFlight.current = false;
            draftRefreshInFlight.current = false;
            setStatus("needs_input");
          }
        } catch (error) {
          repairInFlight.current = false;
          draftRefreshInFlight.current = false;
          setStatus("needs_input");
          current.onError(error);
        }
      })();
    };
    const onAssistantSettled = (event: Event) => {
      const detail = (event as CustomEvent<{ id?: string }>).detail;
      if (detail?.id !== optionsRef.current.agentId || !repairInFlight.current) return;
      window.setTimeout(() => {
        if (!repairInFlight.current || draftRefreshInFlight.current) return;
        repairInFlight.current = false;
        setStatus("needs_input");
      }, 800);
    };
    window.addEventListener(AGENT_DRAFT_CHANGED_EVENT, onDraftChanged);
    window.addEventListener(ASSISTANT_SETTLED_EVENT, onAssistantSettled);
    return () => {
      window.removeEventListener(AGENT_DRAFT_CHANGED_EVENT, onDraftChanged);
      window.removeEventListener(ASSISTANT_SETTLED_EVENT, onAssistantSettled);
    };
  }, [qc]);

  return { status, changedPaths, reset, onManualEdit, markSaved, setValidationResult, markReady, preparePublish };
}
