import { redactSecretString } from "./agent-secret-redaction";

export function safeErrorMessage(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error ?? "Unknown error");
  return redactSecretString(message);
}
