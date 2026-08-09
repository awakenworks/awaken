import type { Dream, DreamInput, ManagedModel, Session } from "./api/types";

export function dreamMemoryStoreId(dream: Dream): string | undefined {
  return (dream.inputs.find((input): input is Extract<DreamInput, { type: "memory_store" }> =>
    input.type === "memory_store"))?.memory_store_id;
}

export function dreamSessionIds(dream: Dream): string[] {
  return (dream.inputs.find((input): input is Extract<DreamInput, { type: "sessions" }> =>
    input.type === "sessions"))?.session_ids ?? [];
}

export function eligibleDreamSessions(sessions: readonly Session[]): Session[] {
  return sessions.filter((session) =>
    session.status !== "running"
      && session.metadata?.["awaken.session.origin"] !== "dream"
      && !session.archived_at,
  );
}

export function readyDreamModels(
  supported: readonly string[],
  executable?: readonly ManagedModel[],
): string[] {
  if (!executable) return [];
  const ready = new Set(executable.flatMap((model) => [model.id, model.display_name]));
  return supported.filter((model) => ready.has(model));
}

export function isDreamTerminal(status: Dream["status"]): boolean {
  return status === "completed" || status === "failed" || status === "canceled";
}
