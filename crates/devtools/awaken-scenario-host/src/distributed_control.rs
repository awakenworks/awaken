//! Deterministic model adapter over the production split-Control composition.
//!
//! This module adds no alternate Control service. It loads the same typed
//! deployment as the product command and supplies only the existing model
//! publication SPI so a provider-free cluster can exercise the real
//! Control-to-Coordinator boundaries.

use std::sync::Arc;

use axum::Router;

use crate::model_publication::{ScenarioHostModelResolver, scenario_model_catalog};

pub async fn build_distributed_control_router() -> Router {
    let deployment = awaken_cli::config::ResolvedDeployment::load(
        awaken_cli::config::ConfigOverrides::default(),
    )
    .unwrap_or_else(|error| panic!("distributed Control deployment configuration: {error}"));
    assert_eq!(
        deployment.role,
        awaken_cli::config::Role::Control,
        "distributed Control scenario requires role = \"control\""
    );
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("distributed Control seal key: {error}"));
    let resolver = Arc::new(ScenarioHostModelResolver::new(
        scenario_model_catalog("adr71-echo").await,
    ));
    awaken_cli::build_control_router_with_publication_resolver(&deployment, &key, resolver)
        .await
        .unwrap_or_else(|error| panic!("assemble distributed Control scenario: {error}"))
}
