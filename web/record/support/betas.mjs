// Recording-side API calls must opt into the same route-specific protocols as
// the console client. Memory and Skills are exclusive families; do not combine
// them with the Managed Agents beta on those routes.
export const MEMORY_HEADERS = { "anthropic-beta": "agent-memory-2026-07-22" };
export const SKILLS_HEADERS = { "anthropic-beta": "skills-2025-10-02" };
export const MANAGED_HEADERS = { "anthropic-beta": "managed-agents-2026-04-01" };
