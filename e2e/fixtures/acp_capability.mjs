/**
 * Wait for the one publication-admission fact shared by projected and live ACP
 * E2Es. TCP/API readiness is deliberately insufficient: a Worker heartbeat must
 * publish detected and negotiated evidence for the exact backend first.
 */
export async function waitForVerifiedAcpCapability(
  baseURL,
  cli,
  { timeoutMs = 120_000, pollMs = 100, requireAvailableLogin = false } = {},
) {
  const deadline = performance.now() + timeoutMs;
  let lastRuntime;
  let lastStatus;
  let lastError;
  while (performance.now() < deadline) {
    try {
      const response = await fetch(`${baseURL}/v1/capabilities`);
      lastStatus = response.status;
      if (response.ok) {
        const value = await response.json();
        lastRuntime = value.runtimes?.find((candidate) => candidate.id === `acp:${cli}`);
        if (
          lastRuntime?.local?.detected === true
          && lastRuntime.local.negotiated !== null
          && (!requireAvailableLogin || lastRuntime.local.login_state === 'available')
        ) return lastRuntime;
      }
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, pollMs));
  }
  throw new Error(
    `ACP worker ${cli} did not publish fresh verified capability evidence: `
      + `status=${lastStatus ?? 'unreachable'} runtime=${JSON.stringify(lastRuntime)} `
      + `error=${lastError ? String(lastError) : 'none'}`,
  );
}
