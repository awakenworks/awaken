//! Anthropic Managed response/request context derived at the IAM policy edge.
//! Identity and workspace authority remain in IAM; this module only projects
//! their typed, non-secret protocol representation.

use awaken_iam_contract::PrincipalRef;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

pub(super) fn memory_actor(
    principal: &PrincipalRef,
) -> awaken_protocol_managed::types::memory::AuthenticatedMemoryActor {
    use awaken_resource_contract::MemoryActor;
    let actor = match principal {
        PrincipalRef::Account { account_id } => MemoryActor::UserActor {
            user_id: account_id.0.clone(),
        },
        PrincipalRef::Service { service_id } => MemoryActor::ServiceAccountActor {
            service_account_id: service_id.clone(),
        },
        PrincipalRef::ApiToken { token_id } => MemoryActor::ApiActor {
            api_key_id: token_id.clone(),
        },
    };
    awaken_protocol_managed::types::memory::AuthenticatedMemoryActor(actor)
}

pub(super) fn with_workspace_header(mut response: Response, workspace: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(workspace) {
        response
            .headers_mut()
            .insert("anthropic-workspace-id", value);
    }
    response
}

/// No-login projection of the same response contract. In this deployment mode
/// there is no authenticated IAM workspace to stamp, so composition supplies
/// its one configured workspace authority.
pub(crate) async fn fixed_workspace_header_guard(
    State(workspace): State<String>,
    req: Request,
    next: Next,
) -> Response {
    with_workspace_header(next.run(req).await, &workspace)
}

#[cfg(test)]
mod tests {
    use awaken_iam_contract::{AccountId, PrincipalRef};
    use awaken_resource_contract::MemoryActor;

    use super::memory_actor;

    #[test]
    fn iam_principals_lower_once_to_managed_memory_actors() {
        // Cause/effect graph: C1 API token, C2 human account, C3 service account.
        // Effects: E1 api_actor, E2 user_actor, E3 service_account_actor with the
        // original non-secret identifier. Rules R1=C1->E1, R2=C2->E2, R3=C3->E3.
        // The IAM principal stays authoritative; this is only its protocol context.
        let cases = [
            (
                PrincipalRef::ApiToken {
                    token_id: "api_1".into(),
                },
                MemoryActor::ApiActor {
                    api_key_id: "api_1".into(),
                },
            ),
            (
                PrincipalRef::Account {
                    account_id: AccountId("user_1".into()),
                },
                MemoryActor::UserActor {
                    user_id: "user_1".into(),
                },
            ),
            (
                PrincipalRef::Service {
                    service_id: "svac_1".into(),
                },
                MemoryActor::ServiceAccountActor {
                    service_account_id: "svac_1".into(),
                },
            ),
        ];
        for (principal, expected) in cases {
            assert_eq!(memory_actor(&principal).0, expected);
        }
    }
}
