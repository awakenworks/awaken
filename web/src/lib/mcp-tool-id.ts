/** The console's side of the `mcp__<server>__<tool>` tool-identity grammar.
 *
 * The authority is `crates/contract/awaken-agent-contract/src/mcp_tool_id.rs`;
 * this is the same encoding, kept honest by the shared vectors in
 * `mcp-tool-id.test.ts`. The console needs it because it rewrites authored tool
 * references when an MCP server is renamed or removed, and those references are
 * the *encoded* ids the runtime will actually see.
 *
 * The encoding is injective, so two differently punctuated server names can
 * never share a namespace:
 *   - an ASCII alphanumeric character is itself;
 *   - `_` is itself, unless the next source character is also `_` (a literal
 *     `__` would forge the separator), in which case it is `-5f`;
 *   - every other byte, `-` included, is `-` plus its two lowercase hex digits,
 *     per UTF-8 byte.
 * Names of ASCII alphanumerics and single underscores — `calc`,
 * `awaken_session`, `my_server` — pass through unchanged.
 */

const ALPHANUMERIC = /^[A-Za-z0-9]$/;

/** Encode one raw server or tool name into the identity charset. */
export function encodeMcpComponent(raw: string): string {
  const bytes = new TextEncoder().encode(raw);
  let out = "";
  for (let index = 0; index < bytes.length; index += 1) {
    const byte = bytes[index];
    const char = String.fromCharCode(byte);
    if (ALPHANUMERIC.test(char)) {
      out += char;
    } else if (byte === 0x5f && bytes[index + 1] !== 0x5f) {
      out += "_";
    } else {
      out += `-${byte.toString(16).padStart(2, "0")}`;
    }
  }
  return out;
}

/** The canonical id of `toolName` on `serverName`, or `null` for an empty half. */
export function mcpToolId(serverName: string, toolName: string): string | null {
  if (serverName === "" || toolName === "") return null;
  return `mcp__${encodeMcpComponent(serverName)}__${encodeMcpComponent(toolName)}`;
}

/** The id prefix every tool on `serverName` shares, or `null` for an empty name. */
export function mcpToolNamespace(serverName: string): string | null {
  if (serverName === "") return null;
  return `mcp__${encodeMcpComponent(serverName)}__`;
}
