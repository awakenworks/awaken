//! Repository projection adapter for a realized Session environment.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use super::{SessionEnvironment, container_repositories};

#[async_trait]
impl pc::RepositoryRealizer for SessionEnvironment {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir { .. } | Self::Namespace { .. } if credential.is_some() => {
                Err(pc::SandboxError::unsupported(
                    "credentialed Repository effects require a container Sandbox",
                ))
            }
            Self::Workdir { sandbox, .. } => {
                pc::RepositoryRealizer::realize_repository(sandbox.as_ref(), plan, credential).await
            }
            Self::Namespace { sandbox, .. } => sandbox.provision_repo(plan, credential),
            Self::Container {
                sandbox,
                hand,
                clock,
                ..
            } => {
                container_repositories::provision(
                    sandbox.as_ref(),
                    hand.binary_path(),
                    plan,
                    credential,
                    clock,
                )
                .await
            }
        }
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        match self {
            Self::Workdir { .. } | Self::Namespace { .. } if credential.is_some() => Err(
                pc::RepositoryPublicationError::Unavailable(pc::SandboxError::unsupported(
                    "credentialed Repository effects require a container Sandbox",
                )),
            ),
            Self::Workdir { sandbox, .. } => {
                pc::RepositoryRealizer::publish_repository(
                    sandbox.as_ref(),
                    plan,
                    expectation,
                    credential,
                )
                .await
            }
            Self::Namespace { sandbox, .. } => sandbox.push_repo(plan, expectation, credential),
            Self::Container {
                sandbox,
                hand,
                clock,
                ..
            } => {
                container_repositories::push(
                    sandbox.as_ref(),
                    hand.binary_path(),
                    plan,
                    expectation,
                    credential,
                    clock,
                )
                .await
            }
        }
    }
}
