//! NATS KV helper predicates.

pub(crate) fn is_tombstone(entry: &async_nats::jetstream::kv::Entry) -> bool {
    matches!(
        entry.operation,
        async_nats::jetstream::kv::Operation::Delete | async_nats::jetstream::kv::Operation::Purge
    )
}
