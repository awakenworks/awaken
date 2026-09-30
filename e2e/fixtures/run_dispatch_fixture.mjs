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

// Credential-only worker scenarios need an ordinary Run, not a Session command.
// Preserve the seed's exact Resource envelope: it is the existing proof that a
// cold Worker may realize Empty -> desired even when desired is empty. The
// frozen Runtime projection is also an execution input, not Session-command
// identity; `detachSessionCommandFixture` is the single ownership boundary for
// both values. Removing either used to work only because background admission
// eagerly created a sandbox before enqueue, which made this fixture depend on a
// second physical-effect owner.
export function ordinaryRunDispatchFixture(seed, runId, threadId) {
  // Causes: C1 the seed carries frozen Session Resources/Runtime; C2 a caller
  // requests a different Thread. Effect: E1 refuse the copy before enqueue,
  // because those envelopes still require the original Session Work authority.
  // Rules C1+!C2=>reuse the same Session; C1+C2=>E1. The Rust dispatch contract
  // remains the runtime admission authority for all other Run shapes.
  if ((seed.session_resources || seed.session_runtime) && seed.activation.thread_id !== threadId) {
    throw new Error('frozen Session projection cannot be copied to another Thread');
  }
  const request = detachSessionCommandFixture(seed);
  request.activation.run_id = runId;
  request.activation.thread_id = threadId;
  return request;
}
