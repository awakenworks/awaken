// Shared E2E encoding of awaken-agent-contract::stable_fingerprint. Callers
// own their ordered tuple and field order; this does not canonicalize JSON,
// validate authorization, or become a product identity implementation.
// The credential-materialization and resource-recovery E2Es validate the two
// domain encodings against their actual Rust consumers, including rejection of
// changed credential coordinates and recovery of the exact cleanup effect.
export function stableFingerprintFixture(value) {
  const bytes = Buffer.from(JSON.stringify(value));
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return `fnv1a64:${hash.toString(16).padStart(16, '0')}`;
}
