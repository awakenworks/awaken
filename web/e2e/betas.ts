// Direct API calls in browser tests opt into the same route-specific protocol
// families as the Console client. These headers are intentionally not combined.
export const MEMORY_HEADERS = { "anthropic-beta": "agent-memory-2026-07-22" };
export const SKILLS_HEADERS = { "anthropic-beta": "skills-2025-10-02" };
export const MANAGED_HEADERS = { "anthropic-beta": "managed-agents-2026-04-01" };
