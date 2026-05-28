use std::collections::BTreeMap;

use awaken_runtime_contract::contract::versioned_registry::PinnedRegistryEntry;
use awaken_runtime_contract::registry_spec::ModelSpec;
use awaken_runtime_contract::{REGISTRY_KIND_MODEL, REGISTRY_KIND_PROVIDER, registry_content_hash};
use serde::Serialize;

pub(super) fn collect_model_manifest_entries(
    entries: &mut BTreeMap<(String, String), PinnedRegistryEntry>,
    model_id: &str,
    model: ModelSpec,
) {
    let provider_id = model.provider_id.clone();
    let model_spec = ModelSpec::new(
        model_id.to_string(),
        provider_id.clone(),
        model.upstream_model,
    );
    insert_manifest_entry(entries, REGISTRY_KIND_MODEL, &model_spec.id, &model_spec);
    insert_manifest_entry(
        entries,
        REGISTRY_KIND_PROVIDER,
        &provider_id,
        &serde_json::json!({ "id": provider_id }),
    );
}

pub(super) fn insert_manifest_entry<T: Serialize>(
    entries: &mut BTreeMap<(String, String), PinnedRegistryEntry>,
    kind: &str,
    id: &str,
    value: &T,
) {
    entries
        .entry((kind.to_string(), id.to_string()))
        .or_insert_with(|| PinnedRegistryEntry {
            kind: kind.to_string(),
            id: id.to_string(),
            version: 1,
            content_hash: runtime_content_hash(kind, id, value),
        });
}

fn runtime_content_hash<T: Serialize>(kind: &str, id: &str, value: &T) -> String {
    let value = serde_json::to_value(value)
        .unwrap_or_else(|_| serde_json::json!({ "kind": kind, "id": id }));
    registry_content_hash(1, &value)
        .map(|(hash, _)| hash)
        .unwrap_or_else(|_| format!("runtime:{kind}/{id}"))
}
