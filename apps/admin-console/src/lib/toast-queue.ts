export type ToastTone = "success" | "error" | "info";

export interface Toast {
  id: string;
  tone: ToastTone;
  message: string;
  detail?: string;
  durationMs: number;
  createdAt: number;
}

export interface ToastInput {
  tone: ToastTone;
  message: string;
  detail?: string;
  durationMs?: number;
}

export const DEFAULT_DURATIONS_MS: Record<ToastTone, number> = {
  success: 3500,
  info: 4500,
  error: 7000,
};

export const MAX_VISIBLE_TOASTS = 5;

export function createToast(input: ToastInput, id: string, now: number): Toast {
  const durationMs =
    typeof input.durationMs === "number" && input.durationMs >= 0
      ? input.durationMs
      : DEFAULT_DURATIONS_MS[input.tone];
  return {
    id,
    tone: input.tone,
    message: input.message,
    detail: input.detail,
    durationMs,
    createdAt: now,
  };
}

export interface AppendResult {
  queue: Toast[];
  displaced: number;
}

export function appendToast(
  queue: Toast[],
  toast: Toast,
  currentDisplaced = 0,
): AppendResult {
  const next = [...queue, toast];
  if (next.length > MAX_VISIBLE_TOASTS) {
    const evicted = next.length - MAX_VISIBLE_TOASTS;
    return {
      queue: next.slice(evicted),
      displaced: currentDisplaced + evicted,
    };
  }
  return { queue: next, displaced: currentDisplaced };
}

export function dismissToast(queue: Toast[], id: string): Toast[] {
  return queue.filter((toast) => toast.id !== id);
}

export function expireToasts(queue: Toast[], now: number): Toast[] {
  return queue.filter((toast) => {
    if (toast.durationMs === 0) {
      return true;
    }
    return now - toast.createdAt < toast.durationMs;
  });
}

export function nextExpiryDelay(queue: Toast[], now: number): number | null {
  let soonest: number | null = null;
  for (const toast of queue) {
    if (toast.durationMs === 0) continue;
    const remaining = toast.createdAt + toast.durationMs - now;
    const safe = remaining < 0 ? 0 : remaining;
    if (soonest === null || safe < soonest) {
      soonest = safe;
    }
  }
  return soonest;
}
