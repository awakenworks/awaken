// @formal entrypoint sandbox.docker_absence_requires_explicit_backend_not_found
//! One Docker deletion boundary shared by ordinary, exact-replacement and
//! unreceipted-scope cleanup. Callers retain their existing authority guards.

use super::*;
use awaken_container_contract::RuntimeError;
use std::time::Duration;

const REMOVAL_TIMEOUT: Duration = Duration::from_secs(30);
const REMOVAL_OBSERVATION_INTERVAL: Duration = Duration::from_millis(25);

impl DockerRuntime {
    /// A successful remove acknowledgment or an exact immutable-ID absence is
    /// deletion evidence. A 409 alone is not: join only an observed Removing
    /// incarnation, retaining typed refusal/unknown outcomes otherwise.
    pub(super) async fn remove_confirmed_incarnation(
        &self,
        container_id: &str,
        force: bool,
    ) -> Result<(), RuntimeError> {
        if container_id.is_empty() {
            return Err(backend("Docker removal requires an immutable incarnation"));
        }
        let operation = async {
            let conflict = match self
                .docker
                .remove_container(
                    container_id,
                    Some(RemoveContainerOptions {
                        force,
                        // An image that declares VOLUME gets an anonymous volume
                        // per container; without this it outlives every session.
                        v: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) if docker_not_found(&error) => return Ok(()),
                Err(
                    error @ bollard::errors::Error::DockerResponseServerError {
                        status_code: 409,
                        ..
                    },
                ) => error,
                Err(error) => return Err(backend(error)),
            };
            loop {
                let observed = match self.docker.inspect_container(container_id, None).await {
                    Ok(observed) => observed,
                    Err(error) if docker_not_found(&error) => return Ok(()),
                    Err(error) => return Err(backend(error)),
                };
                if observed.id.as_deref() != Some(container_id) {
                    return Err(backend(
                        "Docker removal observation changed immutable identity",
                    ));
                }
                if observed.state.as_ref().and_then(|state| state.status)
                    != Some(ContainerStateStatusEnum::REMOVING)
                {
                    return Err(backend(conflict));
                }
                // The daemon still owns this exact object. Do not release its
                // dependents until a later exact-ID query positively finds it
                // absent; transport failure and a running/exited object are not
                // that receipt. No second delete or mutable-name query occurs.
                tokio::time::sleep(REMOVAL_OBSERVATION_INTERVAL).await;
            }
        };
        tokio::time::timeout(REMOVAL_TIMEOUT, operation)
            .await
            .map_err(|_| {
                RuntimeError::MayHaveCommitted(
                    "Docker exact removal has no completion receipt within its deadline".into(),
                )
            })?
    }
}

#[cfg(test)]
mod tests;
