//! Canonical Kubernetes projection and quantity codec for Pod resource budgets.

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

/// The Pod container's native CPU, memory, and ephemeral-storage reservations
/// and limits. Kubernetes scheduling consumes requests; cgroups enforce limits.
pub fn pod_resources(
    requests: &pc::ResourceRequests,
    limits: &pc::ResourceLimits,
) -> Option<ResourceRequirements> {
    if !requests.is_set() && !limits.is_set() {
        return None;
    }
    let mut projected_requests = BTreeMap::new();
    if let Some(cpu) = requests.cpu_millis {
        projected_requests.insert("cpu".to_string(), cpu_quantity(u64::from(cpu)));
    }
    if let Some(memory) = requests.memory_bytes {
        projected_requests.insert("memory".to_string(), byte_quantity(memory));
    }
    if let Some(disk) = requests.disk_bytes {
        projected_requests.insert("ephemeral-storage".to_string(), byte_quantity(disk));
    }
    let mut projected_limits = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        projected_limits.insert("cpu".to_string(), cpu_quantity(u64::from(cpu)));
    }
    if let Some(memory) = limits.memory_bytes {
        projected_limits.insert("memory".to_string(), byte_quantity(memory));
    }
    if let Some(disk) = limits.disk_bytes {
        projected_limits.insert("ephemeral-storage".to_string(), byte_quantity(disk));
    }
    if projected_requests.is_empty() && projected_limits.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        requests: (!projected_requests.is_empty()).then_some(projected_requests),
        limits: (!projected_limits.is_empty()).then_some(projected_limits),
        claims: None,
    })
}

/// Kubernetes' canonical CPU text. The apiserver echoes whole cores without the
/// milli suffix (`4000m` reads back as `4`), and the immutable Pod projection
/// is compared verbatim on restore and cleanup.
pub fn cpu_quantity(millis: u64) -> Quantity {
    if millis.is_multiple_of(1_000) {
        Quantity((millis / 1_000).to_string())
    } else {
        Quantity(format!("{millis}m"))
    }
}

const DECIMAL_SUFFIXES: [&str; 6] = ["k", "M", "G", "T", "P", "E"];

/// Kubernetes' canonical decimal byte text (`8000000000` reads back as `8G`).
pub fn byte_quantity(bytes: u64) -> Quantity {
    let mut value = bytes;
    let mut suffix = "";
    for next in DECIMAL_SUFFIXES {
        if value == 0 || !value.is_multiple_of(1_000) {
            break;
        }
        value /= 1_000;
        suffix = next;
    }
    Quantity(format!("{value}{suffix}"))
}

pub fn quantity_cpu_millis(quantity: &Quantity) -> Option<u64> {
    match quantity.0.strip_suffix('m') {
        Some(millis) => millis.parse().ok(),
        None => quantity.0.parse::<u64>().ok()?.checked_mul(1_000),
    }
}

pub fn quantity_bytes(quantity: &Quantity) -> Option<u64> {
    let text = quantity.0.as_str();
    let split = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    let scale = if suffix.is_empty() {
        1
    } else {
        let exponent = DECIMAL_SUFFIXES.iter().position(|known| *known == suffix)?;
        1_000_u64.checked_pow(u32::try_from(exponent).ok()? + 1)?
    };
    digits.parse::<u64>().ok()?.checked_mul(scale)
}
