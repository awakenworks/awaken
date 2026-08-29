// Remove only Session-command ownership from a server-authored seed. The
// returned dispatch keeps neutral execution inputs (including an optional
// Resource/Runtime projection) while generic enqueue regains full-dispatch
// identity. Production admission remains the validity authority.
export function detachSessionCommandFixture(seed) {
  const request = structuredClone(seed);
  delete request.identity_scope;
  delete request.session_thread_id;
  delete request.session_activity_epoch;
  delete request.session_run_replacement;
  return request;
}

// Credential-only worker scenarios need an ordinary Run, not merely a generic
// dispatch that still asks a Worker to realize Session-owned inputs.
export function ordinaryRunDispatchFixture(seed, runId, threadId) {
  const request = detachSessionCommandFixture(seed);
  request.activation.run_id = runId;
  request.activation.thread_id = threadId;
  delete request.session_resources;
  delete request.session_runtime;
  return request;
}
