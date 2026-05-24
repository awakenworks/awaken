import { useRef, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { ConfigApiError, classifyEvalError, evalApi, type EvalRun } from "@/lib/api";
import { useDialogKeys } from "@/lib/use-dialog-keys";

/** Test a model binding with a single prompt.
 *
 *  Wraps `POST /v1/eval/online { user_input, models: [modelId] }`. The
 *  online eval endpoint runs the prompt through the real provider (no
 *  scripted shortcut), captures the response into an `EvalRun`, and
 *  returns it synchronously. We surface just the final assistant text
 *  and basic numbers — operators wanting deep observability still go
 *  through the agent editor's preview chat. */
export function ModelTestModal({
  modelId,
  onClose,
}: {
  modelId: string;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [prompt, setPrompt] = useState("");
  const [response, setResponse] = useState<{ text: string; raw: boolean } | null>(null);
  const [runMeta, setRunMeta] = useState<EvalRun | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Operator-controlled budget — defaults are conservative so a
  // misconfigured provider can't sit on the request forever or burn an
  // unbounded token bill. Token cap is post-hoc (server-side), wall
  // time cap is enforced server-side per cell.
  const [maxWalltimeSecs, setMaxWalltimeSecs] = useState(30);
  const [maxTotalTokens, setMaxTotalTokens] = useState(2000);
  const dialogRef = useRef<HTMLDivElement>(null);
  useDialogKeys({ dialogRef, onClose });

  const sendPrompt = useMutation({
    // Form values are frozen into the mutation variables at submit, not
    // read from React state inside `mutationFn`. Otherwise editing the
    // prompt while a request is in flight would make the resolved
    // response describe a prompt different from the one that was sent.
    mutationFn: (vars: { prompt: string; maxWalltimeSecs: number; maxTotalTokens: number }) =>
      evalApi.online({
        user_input: vars.prompt,
        models: [modelId],
        // Skip persist so a test ping doesn't pollute the eval-run
        // store. Server defaults persist=false when omitted (see
        // OnlineEvalRequest doc) — but be explicit so a future server
        // default flip doesn't surprise us.
        persist: false,
        max_walltime_secs: vars.maxWalltimeSecs,
        max_total_tokens: vars.maxTotalTokens,
      }),
    onSuccess: (resp) => {
      setError(null);
      setRunMeta(resp.run);
      // Read the assistant's final text from the canonical wire field
      // `item.report.final_text`. `pickAssistantText` no longer
      // deep-searches arbitrary `text` keys — that could pick up the
      // user prompt, a tool argument, or any trace fragment.
      setResponse(pickAssistantText(resp.run));
    },
    onError: (err) => {
      // 503 = route exists but eval-run store is unreachable. Distinct
      // from "feature disabled" so operators don't go chasing a flag
      // when the real fix is a wiring/disk issue.
      const cat = classifyEvalError(err);
      const msg =
        cat === "disabled"
          ? t("evalRuns.disabledTitle") + " — " + t("evalRuns.disabledHint")
          : cat === "store_error"
            ? t("evalRuns.storeUnreachableTitle") + " — " + t("evalRuns.storeUnreachableHint")
            : err instanceof ConfigApiError
              ? err.message
              : err instanceof Error
                ? err.message
                : String(err);
      setError(msg);
      setResponse(null);
      setRunMeta(null);
    },
  });

  // Typed check + non-negative clamp: a `started_at_secs` of 0
  // (epoch / unset sentinel) would be falsy, hiding the duration; and
  // an `ended_at_secs < started_at_secs` (clock skew or out-of-order
  // event delivery) would render a negative ms.
  const elapsedMs = (() => {
    const s = runMeta?.started_at_secs;
    const e = runMeta?.ended_at_secs;
    if (typeof s !== "number" || typeof e !== "number") return null;
    return Math.max(0, Math.round((e - s) * 1000));
  })();
  const failureCount = runMeta?.items
    ? runMeta.items.filter((it) => it.report?.passed === false).length
    : 0;

  return (
    <div
      ref={dialogRef}
      role="dialog"
      aria-modal="true"
      aria-label={t("modelTest.title")}
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!prompt.trim() || sendPrompt.isPending) return;
          sendPrompt.mutate({ prompt: prompt.trim(), maxWalltimeSecs, maxTotalTokens });
        }}
        className="flex max-h-[85vh] w-full max-w-2xl flex-col rounded-sm border border-line bg-surface p-5 shadow-card"
      >
        <header>
          <h2 className="text-lg font-semibold text-fg-strong">
            {t("modelTest.title")} · <span className="font-mono">{modelId}</span>
          </h2>
          <p className="mt-1 text-xs text-fg-soft">{t("modelTest.description")}</p>
        </header>

        <p
          role="note"
          className="mt-3 rounded-sm border border-tone-warn/30 bg-tone-warn/[0.06] px-3 py-2 text-[11px] text-fg-soft"
        >
          <span className="font-medium text-tone-warn">{t("modelTest.costLabel")}:</span>{" "}
          {t("modelTest.costHint")}
        </p>

        <label className="mt-4 block">
          <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
            {t("modelTest.promptLabel")}
          </span>
          <textarea
            autoFocus
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            placeholder={t("modelTest.promptPlaceholder")}
            rows={3}
            disabled={sendPrompt.isPending}
            className="mt-1 w-full resize-y rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm focus:border-fg focus:outline-none disabled:opacity-60"
          />
        </label>

        <div className="mt-3 grid grid-cols-2 gap-3">
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("modelTest.maxWalltimeLabel")}
            </span>
            <input
              type="number"
              min={1}
              max={600}
              value={maxWalltimeSecs}
              onChange={(e) =>
                setMaxWalltimeSecs(Math.max(1, Math.min(600, Number(e.target.value) || 30)))
              }
              disabled={sendPrompt.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
          </label>
          <label className="block">
            <span className="text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
              {t("modelTest.maxTokensLabel")}
            </span>
            <input
              type="number"
              min={1}
              max={200000}
              value={maxTotalTokens}
              onChange={(e) =>
                setMaxTotalTokens(Math.max(1, Math.min(200000, Number(e.target.value) || 2000)))
              }
              disabled={sendPrompt.isPending}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm focus:border-fg focus:outline-none disabled:opacity-60"
            />
          </label>
        </div>

        {response !== null && (
          <section className="mt-4 min-h-0 flex-1 overflow-auto rounded-sm border border-line bg-soft p-3">
            <h3 className="mb-1 text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
              {response.raw ? t("modelTest.rawItem") : t("modelTest.response")}
            </h3>
            <pre
              className="whitespace-pre-wrap break-words text-sm text-fg"
              data-testid="model-test-response"
              data-raw={response.raw ? "true" : "false"}
            >
              {response.text || "(empty)"}
            </pre>
            <div className="mt-3 flex flex-wrap gap-3 text-[11px] text-fg-soft">
              {elapsedMs !== null && (
                <span>{t("modelTest.elapsed", { ms: elapsedMs.toLocaleString() })}</span>
              )}
              {failureCount > 0 && (
                <span className="text-tone-error">
                  {t("modelTest.failures")}: {failureCount}
                </span>
              )}
            </div>
          </section>
        )}

        {error && (
          <div
            role="alert"
            className="mt-4 rounded-sm border border-tone-error/30 bg-tone-error/[0.06] p-3 text-sm text-tone-error"
            data-testid="model-test-error"
          >
            {error}
          </div>
        )}

        <div className="mt-5 flex justify-end gap-2">
          {/* Close stays enabled even while pending so the operator
              can bail out. Note: the in-flight server request will
              continue (no AbortController plumbed through the eval/
              online endpoint yet) — closing only drops the client's
              wait. The mutation result, if any, will be discarded. */}
          <button
            type="button"
            onClick={onClose}
            className="rounded-sm border border-line-strong px-3 py-1.5 text-sm font-medium text-fg transition hover:bg-soft"
          >
            {sendPrompt.isPending ? t("modelTest.cancel") : t("common.close")}
          </button>
          <button
            type="submit"
            disabled={!prompt.trim() || sendPrompt.isPending}
            data-testid="model-test-send"
            className="rounded-sm bg-accent px-4 py-1.5 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {sendPrompt.isPending ? t("modelTest.sending") : t("modelTest.send")}
          </button>
        </div>
      </form>
    </div>
  );
}

/** Pick the assistant's final text out of an eval run. The wire
 *  contract is `EvalRunItem.report.final_text` — read that first.
 *  We only fall back to the raw item JSON (as `{ raw }`) when the
 *  field is genuinely absent (e.g., a partial / failed run). The
 *  earlier version deep-searched any nested `text`/`final_text` key,
 *  which could pick up the user prompt, a tool argument, or any
 *  intermediate trace text and show it as the assistant's response. */
function pickAssistantText(run: EvalRun): { text: string; raw: boolean } {
  const items = Array.isArray(run.items) ? run.items : [];
  for (const item of items) {
    const finalText = item?.report?.final_text;
    if (typeof finalText === "string" && finalText.length > 0) {
      return { text: finalText, raw: false };
    }
  }
  // Genuine fallback: surface the first item as-is so the operator can
  // diagnose why `report.final_text` was empty (no successful inference,
  // tool-only turn, error). Marked `raw` so the UI labels it.
  if (items.length > 0) {
    return { text: JSON.stringify(items[0], null, 2), raw: true };
  }
  return { text: "", raw: false };
}
