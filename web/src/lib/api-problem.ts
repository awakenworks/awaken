import { ApiClientError } from "./api/client";

export type ApiProblemAction =
  | "authenticate"
  | "authorize"
  | "refresh_conflict"
  | "retry_dependency"
  | "inspect";

export interface ApiProblemPresentation {
  action: ApiProblemAction;
  code: string;
  detail: string;
  requestId?: string;
}

/** Preserve protocol semantics at the UI boundary. Product surfaces translate
 * the action label, but never collapse authentication, authorization, conflict,
 * and dependency availability into one generic retry. */
export function presentApiProblem(error: unknown): ApiProblemPresentation {
  if (!(error instanceof ApiClientError)) {
    return {
      action: "inspect",
      code: "client_error",
      detail: error instanceof Error ? error.message : "Unknown error",
    };
  }
  const action: ApiProblemAction = error.status === 401
    ? "authenticate"
    : error.status === 403
      ? "authorize"
      : error.status === 409
        ? "refresh_conflict"
        : error.status === 503
          ? "retry_dependency"
          : "inspect";
  return { action, code: error.code, detail: error.message, requestId: error.requestId };
}
