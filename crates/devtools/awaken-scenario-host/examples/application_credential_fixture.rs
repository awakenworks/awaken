//! Windows qualification fixture: real Native Vault and production delivery
//! router, with synthetic workspace/auth and an ephemeral in-memory repository.
//! Not a production IAM/storage deployment. Secrets are supplied by private
//! input files/HTTP, never command-line arguments or model calls.
use awaken_credential_vault::{InMemorySecretStore, repo::InMemoryCredentialRepo};
use awaken_protocol_managed::VaultState;
use awaken_tenancy::WorkspaceScope;
use axum::{
    Extension,
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use std::{path::PathBuf, sync::Arc};
#[path = "application_credential_fixture/execution.rs"]
mod execution;

async fn auth(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {token}"))
    {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}
use axum::response::IntoResponse;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(target_os = "windows") {
        return Err("local Windows qualification only".into());
    }
    let root = PathBuf::from(std::env::args().nth(1).ok_or("fixture root required")?);
    let token = Arc::new(std::fs::read_to_string(root.join("native-token"))?);
    let clock = awaken_time::system_clock();
    let vault = Arc::new(VaultState::new(
        Arc::new(InMemorySecretStore::new()),
        Arc::new(InMemoryCredentialRepo::new()),
        clock.clone(),
        None,
    ));
    let executions = execution::ExecutionFixture::open(&root, clock.clone()).await?;
    executions.context("a").await?;
    let fault = executions.clone();
    let qualification = axum::Router::new().route(
        "/qualification/execution/{kind}",
        axum::routing::post(
            move |axum::extract::Path(kind): axum::extract::Path<String>| {
                let fixture = fault.clone();
                async move {
                    match fixture.fault(&kind).await {
                        Ok(()) => axum::http::StatusCode::OK,
                        Err(_) => axum::http::StatusCode::CONFLICT,
                    }
                }
            },
        ),
    );
    let app = awaken_control::application_credentials_router(
        vault,
        root.join("consumers.json"),
        Some(executions.authority.clone()),
        clock,
    )
    .merge(qualification)
    .layer(Extension(
        awaken_protocol_managed::types::memory::AuthenticatedMemoryActor(
            awaken_resource_contract::MemoryActor::ServiceAccountActor {
                service_account_id: "application-qualification".into(),
            },
        ),
    ))
    .layer(Extension(WorkspaceScope("workspace-e2e".into())))
    .layer(middleware::from_fn_with_state(token, auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(
        root.join("native-ready.json"),
        format!(
            "{{\"url\":\"http://{}\",\"execution_selector\":{{\"session_id\":\"session_a\",\"run_id\":\"run_a\"}}}}",
            listener.local_addr()?
        ),
    )?;
    axum::serve(listener, app).await?;
    Ok(())
}
