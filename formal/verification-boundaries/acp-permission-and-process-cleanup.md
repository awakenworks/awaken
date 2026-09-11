# ACP permission and process-cleanup verification boundaries

`AcpBoundary` historically placed permission projection and process cleanup in
one finite model, while the coverage row named permission/effect order but cited
only cleanup evidence. These are now separate obligations. The model remains the
single bounded cross-check; no second cleanup or permission authority is added.

## Permission effect order

`jsonrpc::permission::answer_request` remains the sole production owner. It
consumes one allocation-free plan: an open projection executes `ProjectCall`,
`ResolveAuthority`, `SendResponse`; settlement executes `ResolveAuthority`,
`SendResponse`, `NoEffect`. Kani proves these two exact arrays. The production
test observes those effects at the real `RunFactAppender`, `PermissionResolver`
and `Wire` writer, including append and write failures.

The existing observed-call selector, complete-input normalization, alias
consensus and offered-option projection remain independent authoritative
kernels. The new proof sequences their outputs and does not repeat their claims.
It proves neither Tokio polling/cancellation, appender durability, resolver
correctness/progress, peer receipt, remote tool effects nor OS process behavior.

## Process cleanup

The cleanup slice retains its `abstract_model_checked` grade. `AcpBoundary`
checks that returned TERM failure leaves same-handle KILL enabled, only observed
exit permits acknowledgement, and cleanup timeout retains the original observer.
Protocol and executor tables provide finite execution evidence. Provider signal
delivery, process-tree quiescence, owner survival, remote recovery and unbounded
liveness remain external or executable boundaries rather than direct proofs.
