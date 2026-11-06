use awaken_provisioning_contract as pc;

pub(crate) fn effect_fence(
    scope: &str,
    operation: &str,
    owner: &str,
    epoch: u64,
) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(
        format!("{scope}:{operation}"),
        owner,
        format!("{owner}-runtime"),
        epoch,
        u64::MAX,
    )
    .expect("live fixture effect identity is valid")
}

pub(crate) fn disposal_authorization(
    prepared: &pc::SandboxEffectFence,
) -> pc::SandboxDisposalAuthorization {
    disposal_authorization_successor(
        prepared,
        prepared.owner(),
        prepared.runtime_incarnation(),
        prepared.epoch(),
    )
}

pub(crate) fn disposal_authorization_successor(
    prepared: &pc::SandboxEffectFence,
    owner: &str,
    runtime_incarnation: &str,
    epoch: u64,
) -> pc::SandboxDisposalAuthorization {
    let preparation_fingerprint = format!("k8s-fixture-preparation:{}", prepared.operation_id());
    let preparation =
        pc::SandboxDisposalPreparation::new(prepared.clone(), preparation_fingerprint)
            .expect("live fixture preparation is complete");
    let operation_id = preparation
        .operation_id()
        .expect("live fixture disposal identity is stable");
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        owner,
        runtime_incarnation,
        epoch,
        prepared.expires_at_unix_ms(),
    )
    .expect("live fixture authorization successor is valid");
    preparation
        .authorize(successor)
        .expect("live fixture authorization is aggregate-authorized")
}
