use super::*;

pub(super) fn new_physical_incarnation() -> Result<String, pc::SandboxError> {
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random)
        .map_err(|error| err(format!("generate filesystem physical incarnation: {error}")))?;
    Ok(blake3::hash(&random).to_hex().to_string())
}

pub(crate) fn restore_input_fingerprint(checkpoint: &pc::SandboxCheckpointRef) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"awaken-filesystem-restore-input/v1\0");
    for value in [
        checkpoint.id.as_str(),
        checkpoint.format.as_str(),
        checkpoint.digest.as_str(),
        checkpoint.environment_fingerprint.as_str(),
        checkpoint.base_image_fingerprint.as_str(),
        checkpoint.suspend_effect_id.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    for value in [
        checkpoint.size_bytes,
        checkpoint.created_at_unix_ms,
        checkpoint.expires_at_unix_ms,
    ] {
        hasher.update(&value.to_be_bytes());
    }
    hasher.update(&(checkpoint.excluded_mounts.len() as u64).to_be_bytes());
    for value in &checkpoint.excluded_mounts {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub(super) fn validate_removed_restore_source(
    source: &RebuildSourceRecord,
    incoming_restore_fingerprint: &str,
) -> Result<(), pc::SandboxError> {
    let participant = source
        .checkpoint
        .as_ref()
        .ok_or_else(|| err("Removed filesystem tombstone has no checkpoint participant"))?;
    let checkpoint = participant
        .reference
        .as_ref()
        .ok_or_else(|| err("Removed filesystem tombstone has no completed checkpoint receipt"))?;
    let checkpoint_operation_id = participant.operation_id.as_deref().ok_or_else(|| {
        err("Removed filesystem tombstone has no durable checkpoint operation identity")
    })?;
    if source.phase != RealizationPhase::Removed
        || source.terminal_source.is_none()
        || participant.request_fingerprint.trim().is_empty()
        || checkpoint_operation_id.trim().is_empty()
        || checkpoint.suspend_effect_id.trim().is_empty()
        || checkpoint.suspend_effect_id != checkpoint_operation_id
        || restore_input_fingerprint(checkpoint) != incoming_restore_fingerprint
    {
        return Err(err(
            "restore input does not identify the checkpoint participant that removed this filesystem",
        ));
    }
    Ok(())
}
