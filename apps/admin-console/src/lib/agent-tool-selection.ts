export function isToolAllowed(
  allowedTools: string[] | undefined,
  toolId: string,
): boolean {
  return allowedTools ? allowedTools.includes(toolId) : true;
}

export function nextAllowedTools(
  allowedTools: string[] | undefined,
  allToolIds: string[],
  toolId: string,
  checked: boolean,
): string[] | undefined {
  if (checked) {
    if (!allowedTools) {
      return undefined;
    }

    const nextAllowed = Array.from(new Set([...allowedTools, toolId])).filter((id) =>
      allToolIds.includes(id),
    );
    return nextAllowed.length >= allToolIds.length ? undefined : nextAllowed;
  }

  const baseAllowed = allowedTools ?? allToolIds;
  return baseAllowed.filter((id) => id !== toolId);
}
