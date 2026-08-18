# Managed Agents real-LLM evaluation

The deterministic suites prove protocol and state-machine correctness. The live
lane measures whether an actual model can use those capabilities. A live pass is
never used to excuse a deterministic failure, and provider authentication, quota,
or transport failures are reported separately from model-quality failures.

## Causal graph

```text
verified randomized facts + stale/decoy partitions
                  |
       Native, ACP, or A2A boundary
                  |
      Dream / Memory tool lifecycle
                  |
 durable output + Session event history
          |                 |
 exact fact scorer      usage/latency
          +--------+--------+
                   |
          thresholded JSON result
```

`llm_eval_metrics.mjs` prints one line prefixed by `AWAKEN_LLM_EVAL`. Setting
`AWAKEN_EVAL_ARTIFACT_DIR` also writes the versioned JSON document. Exact
`AWKFACT_*` canaries make the subject stochastic but the oracle deterministic.

## Metrics and default gates

| Feature | Metrics | Default gate |
|---|---|---|
| Dream lifecycle | completion, source immutability, output isolation/mutation | all `1.0` |
| Dream quality | fact precision, recall, F1, contamination, stale-fact rate | precision `1.0`; recall/F1 `>=0.8`; contamination/stale `0` |
| Native Memory | write commit, fresh-Session recall, precision/F1, HITL enforcement | all `1.0`; contamination `0` |
| ACP Memory | writer commit, predecessor recall, all-writer recall@k, precision/F1 | all `1.0`; contamination `0`; selected runtime without its exact credential profile or minimum compatible CLI version fails before server launch |
| Operations | p50/p95 latency, retry rate, input/output/cache tokens | p95 bounded; retry and usage reported |

Latency ceilings are deliberately environment-configurable:
`AWAKEN_DREAM_MAX_LATENCY_MS`, `AWAKEN_MEMORY_MAX_LATENCY_MS`, and
`AWAKEN_ACP_MEMORY_MAX_LATENCY_MS`. Quality floors can only be changed explicitly
with `AWAKEN_DREAM_MIN_PRECISION`, `AWAKEN_DREAM_MIN_RECALL`, and
`AWAKEN_DREAM_MIN_F1`; the artifact records the effective threshold.

## Provider/model-reference matrix

Dream and Session overrides consume the existing canonical grammar instead of a
provider allowlist:

```text
model[;provider=P[;api=D[;endpoint=E]]][;executor=native|acp:CLI]
executor=a2a:URL
profile=PROFILE
```

Provider, dialect, endpoint, model, ACP adapter, and A2A URL components are
opaque and percent-encoded when they contain `;`, `=`, or `%`. The dependency
chain is fail closed (`api` requires `provider`; `endpoint` requires both). A2A
is executor-only because the remote Agent Card owns its model selection. The
Rust finite matrix checks 1,323 provider/API/endpoint/executor combinations: every
built-in provider (Anthropic, OpenAI, DeepSeek, Kimi, Gemini, Vertex), every
built-in dialect, AnyRouter and arbitrary third-party providers, plus Native,
Codex, Claude Code, Gemini CLI, OpenCode, Hermes, and A2A. Malformed escapes,
duplicate qualifiers, dependency violations, and canonical round trips are
covered in the same decision table.

The live Provider Connection lane automatically selects configured Anthropic,
OpenAI, DeepSeek, Kimi, Gemini, Vertex, and AnyRouter cases. Third-party provider
identities remain opaque but must select one of the five installed wire dialects;
inventing a dialect cannot create an executor. API-key cases pass only an
environment-variable name in authored JSON, while Vertex uses its server-owned
project/location endpoint plus an OAuth helper, so emitted evidence contains no
credential bytes.

## Runtime matrix and evidence

| Subject | Native | ACP | A2A | Negative/fault layer |
|---|---|---|---|---|
| Dream consolidation | `managed_dream_real_eval_e2e.mjs` uses the production provider executor | canonical `executor=acp:CLI` resolves through ordinary Session publication; deterministic lifecycle remains backend-neutral | executor-only remote agents cannot consume local Dream mounts and fail closed | cancel/archive/provider failure/CAS/terminal-state suites |
| Memory write/recall | `managed_resources_e2e.mjs`, real provider and official SDK | `acp_runtime_memory_matrix_e2e.mjs`: Claude Code, Codex, Gemini CLI, OpenCode, and Hermes writers × readers, each through its catalog-owned process-secret or credential-artifact profile | inbound A2A shares the frozen Session baseline; outbound A2A owns remote memory and rejects local mounts | read-only admission, missing runtime credential, crash, extraction recovery, stale revision suites |
| Tools/HITL | Native Hand semantic suite | ACP JSON-RPC/spill/permission suites | inbound A2A allow/deny/cancel; outbound A2A has no local Hand | malformed args, traversal/symlink, denial, restart failure |
| Score oracle | Node unit tests | same scorer | same scorer for remote outputs when applicable | malformed thresholds/usage, empty percentiles, stale/unknown/duplicate tags |

`conformance/managed_native_acp_causal_coverage_e2e.mjs` is the offline
completeness gate over Native, ACP, A2A, and negative partitions. “A2A” is
deliberately split into inbound protocol compatibility and outbound remote-agent
execution; an inbound A2A request can drive a Native/ACP Session, whereas an
outbound A2A model reference cannot inherit local tools or mounted resources.

Host runtime discovery uses the same lenient version rule as the live matrix:
Claude Code `>=2.1.221`, Codex `>=0.146.0`, Gemini CLI `>=0.53.1`, OpenCode
`>=1.18.12`, and Hermes `>=0.19.0`. Decorated vendor output is accepted and any
newer semantic version remains eligible. Missing, malformed, or older evidence is
reported as `acp_version_unsupported` before login probing and capability
publication; container images continue to use the catalog's exact package pins.

The scorer's finite domain is exhaustively checked by
`llm_eval_metrics_modelcheck.mjs` (256 expected/observed states): every ratio stays
in `[0,1]`, duplicate output is idempotent, and F1 equals the harmonic mean. This
is useful formal verification of the oracle; it does not pretend to prove a
stochastic LLM's future behavior.

## Commands

```sh
npm run test:eval-metrics
npm run test:dream:real
npm run test:real:managed-evals
npm run test:real:providers
npm run test:real:acp-memory-all
```

On a machine without bwrap/Seatbelt or a container runtime, the isolated File
lane correctly refuses to downgrade. The real Memory-only quality lane remains
available with:

```sh
SESSION_DEPLOYMENT_SANDBOX_TIER=local AWAKEN_EVAL_MEMORY_ONLY=1 \
  node managed_resources_e2e.mjs
```

Memory is read-write and its write-through repository remains authoritative.

The live tests load an existing Kimi Code configuration through the repository's
secret-safe configuration helper when process environment variables are absent.
No credential value is written to the evaluation artifact.
