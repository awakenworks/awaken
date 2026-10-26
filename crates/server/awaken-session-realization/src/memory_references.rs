// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: One injected encoder retains Run, Resource-effect and terminal-memory transport scopes.
//! Memory reference transport roles used by Session realization.

/// One injected transport adapter encodes both ordinary Run-scoped and
/// terminal Session-scoped Memory operations. The generic Resource ports stay
/// authoritative; this private intersection trait only permits one trait
/// object to carry both implementations through Runtime composition.
pub trait SessionMemoryReferenceEncoder:
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_dispatch_contract::RunClaim,
    > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_realization_contract::SessionResourceMaterializationIntent,
    > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_state_contract::SessionTerminalMemoryIntent,
    >
{
}

impl<T> SessionMemoryReferenceEncoder for T where
    T: awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_dispatch_contract::RunClaim,
        > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_session_realization_contract::SessionResourceMaterializationIntent,
        > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_session_state_contract::SessionTerminalMemoryIntent,
        >
{
}
