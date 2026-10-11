use super::{
    PlaintextHolder, ResolvedInput, ResolvedInputSource, ResolvedSessionResources,
    ResourceActivationError, SessionResourcesOperations,
};

impl SessionResourcesOperations<'_> {
    /// Install only missing exact credential pins. Both retained slots are
    /// checked before either changes, so a rejected migration is a stutter.
    pub fn migrate_repository_credential_pins(
        &mut self,
        active: ResolvedSessionResources,
        pending: Option<ResolvedSessionResources>,
    ) -> Result<bool, ResourceActivationError> {
        let state = &mut self.aggregate.state;
        let holder = state
            .frozen_baseline()
            .ok_or(ResourceActivationError::CredentialPinMismatch)?
            .environment
            .credential_realization
            .resource_holder
            .clone();
        validate_pin_migration(&state.resources.active, &active, &holder)?;
        match (&state.resources.pending, &pending) {
            (Some(old), Some(next)) => validate_pin_migration(old, next, &holder)?,
            (None, None) => {}
            _ => return Err(ResourceActivationError::CredentialPinMismatch),
        }
        let changed = state.resources.active != active || state.resources.pending != pending;
        if changed {
            state.resources.active = active;
            state.resources.pending = pending;
        }
        Ok(changed)
    }

    /// Rotate one exact desired Repository pin while retaining all topology,
    /// config and unrelated inputs; attempted pending generations cannot change.
    pub fn replace_desired_repository_credential(
        &mut self,
        input: ResolvedInput,
    ) -> Result<(), ResourceActivationError> {
        let state = &mut self.aggregate.state;
        if state.is_terminal() {
            return Err(ResourceActivationError::Terminal);
        }
        let old = state
            .resources
            .desired()
            .inputs()
            .iter()
            .find(|old| old.binding_id == input.binding_id)
            .ok_or(ResourceActivationError::CredentialPinMismatch)?;
        let mut normalized = input.clone();
        let (
            ResolvedInputSource::Repository {
                credential: previous,
                ..
            },
            ResolvedInputSource::Repository {
                config,
                credential: next,
                ..
            },
        ) = (&old.source, &mut normalized.source)
        else {
            return Err(ResourceActivationError::CredentialPinMismatch);
        };
        let holder = &state
            .frozen_baseline()
            .ok_or(ResourceActivationError::CredentialPinMismatch)?
            .environment
            .credential_realization
            .resource_holder;
        let pin = next
            .as_ref()
            .ok_or(ResourceActivationError::CredentialPinMismatch)?;
        let binding = config
            .credential_binding
            .as_deref()
            .ok_or(ResourceActivationError::CredentialPinMismatch)?;
        pin.validate_for_repository(binding, &config.remote_url)
            .map_err(|_| ResourceActivationError::CredentialPinMismatch)?;
        if &pin.selected_plaintext_holder != holder
            || input.updated_at_unix_ms < old.updated_at_unix_ms
            || previous.as_ref().is_some_and(|previous| {
                previous.access.credential.id != pin.access.credential.id
                    || previous.access.credential.revision > pin.access.credential.revision
            })
        {
            return Err(ResourceActivationError::CredentialPinMismatch);
        }
        *next = previous.clone();
        normalized.updated_at_unix_ms = old.updated_at_unix_ms;
        if normalized != *old {
            return Err(ResourceActivationError::CredentialPinMismatch);
        }
        let desired = state
            .resources
            .desired()
            .replace(input)
            .map_err(|_| ResourceActivationError::CredentialPinMismatch)?;
        if state.resources.pending.is_some() {
            state
                .resources
                .revise_unattempted_pending(&state.session_id, desired)?;
        } else {
            state.resources.active = desired;
        }
        Ok(())
    }
}

fn validate_pin_migration(
    old: &ResolvedSessionResources,
    next: &ResolvedSessionResources,
    holder: &PlaintextHolder,
) -> Result<(), ResourceActivationError> {
    let (mut normalized, skills) = next.clone().into_parts();
    if normalized.len() != old.inputs().len() {
        return Err(ResourceActivationError::CredentialPinMismatch);
    }
    for (before, after) in old.inputs().iter().zip(&mut normalized) {
        if let (
            ResolvedInputSource::Repository {
                credential: previous,
                ..
            },
            ResolvedInputSource::Repository {
                config,
                credential: pin,
                ..
            },
        ) = (&before.source, &mut after.source)
        {
            if previous.is_some() && previous != pin {
                return Err(ResourceActivationError::CredentialPinMismatch);
            }
            if let Some(pin) = pin {
                let binding = config
                    .credential_binding
                    .as_deref()
                    .ok_or(ResourceActivationError::CredentialPinMismatch)?;
                pin.validate_for_repository(binding, &config.remote_url)
                    .map_err(|_| ResourceActivationError::CredentialPinMismatch)?;
                if &pin.selected_plaintext_holder != holder {
                    return Err(ResourceActivationError::CredentialPinMismatch);
                }
            } else if config.credential_binding.is_some() {
                return Err(ResourceActivationError::CredentialPinMismatch);
            }
            *pin = previous.clone();
        }
    }
    if ResolvedSessionResources::try_new(normalized, skills)
        .map_err(|_| ResourceActivationError::CredentialPinMismatch)?
        != *old
    {
        return Err(ResourceActivationError::CredentialPinMismatch);
    }
    Ok(())
}
