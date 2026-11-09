# Tool-result image test design

The image read contract returns text or an image through the existing Read tool.
Provider shaping operates on an outbound request copy. It grants no file access,
changes no committed transcript and introduces no second image tool.

## Cases and evidence boundaries

| Case | Owner and expected result |
| --- | --- |
| A route rejects image input | [Provider shaping](../../crates/sdk/providers/awaken-provider-genai/src/tool_images.rs) replaces every tool image with an explicit model-visible error; text and tool correlation remain |
| One Step exceeds eight images | The existing `the_per_turn_limit_serves_calls_in_order_and_refuses_the_rest` case accepts the exact boundary, preserves call order and marks surplus tool results as errors; the held test name retains its protocol wording |
| Whole-request count and byte bounds | The byte-budget and history cases prioritize the latest tool batch, retain newest history within the request limits and give older omissions a plain explanation |
| Retry and model fallback | Shaping the outbound copy is idempotent; later requests may use the original transcript with a different image capability |
| Native Read output | The built-in Read suite checks real fixture files, text/image discrimination, size bounds, resizing and explanatory output |
| Provider wire compatibility | [Wire captures](../../crates/sdk/providers/awaken-provider-genai/tests/tool_schema_compat.rs) exercise the actual adapter serializers for buffered/streaming tool images, tool-result correlation and real Read output |
| Fallible model admission | Image compatibility fixtures construct `ModelBinding` with `parse` and an explicit fixture invariant; they add no panic-based production admission path |
| Public contracts | The pinned-nightly snapshot gate checks ReadOutput, image elision, runtime image capability and the Step/request budgets; examine the semantic diff before accepting generated snapshots |

Wire captures use local fixture endpoints. They verify serialization, not acceptance by
an external provider or a live account. Tests requiring an external service remain a
separate qualification and must not be counted when skipped. File-access policy and
business permissions remain with their existing native owners.

## Execution

Run `scripts/ci/test.sh -p awaken-provider-genai` and
`scripts/ci/test.sh -p awaken-ext-builtin-tools`. These official entry points retain
the authored workspace compilation graph and required companion. Run the normal
boundary, format and commit hooks, then `scripts/ci/check_public_api.sh --require-tools`.
Snapshot regeneration uses the same script with `--bless` and the pinned nightly.
Keep raw execution output outside the repository.
