//! Exact, publication-pinned inference realization port.

use std::sync::Arc;

use crate::CredentialRealizationCapabilities;
use crate::activation::RunActivation;
use crate::llm::LlmExecutor;
use crate::resolved::{Backend, ResolvedModelCandidate};
use crate::runtime_context::RuntimeRunContext;

/// Turns secret-free, admission-pinned inference access into a live executor.
pub trait InferenceExecutorMaterializer: Send + Sync {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[]
    }

    fn credential_realization_capabilities(&self) -> CredentialRealizationCapabilities {
        CredentialRealizationCapabilities::default()
    }

    fn materialize_pinned(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>>;

    fn materialize(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let mut matching = activation
            .snapshot
            .resolved_spec
            .execution_candidates(Some(activation.effective_model_ref()))
            .into_iter();
        let exact = matching.next().ok_or_else(|| {
            format!(
                "model `{}` is outside the publication-pinned candidate set",
                activation.effective_model_ref()
            )
        })?;
        if matching.next().is_some() {
            return Err(format!(
                "model `{}` has multiple publication-pinned bindings; this inference materializer must provide pool-aware exact-binding routing",
                activation.effective_model_ref()
            ));
        }
        if !matches!(
            Backend::from_ref(&exact.binding.backend_ref),
            Backend::Native
        ) {
            return Ok(None);
        }
        self.materialize_pinned(exact, context)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "snapshot `{}` pinned inference access cannot materialize model `{}`",
                    activation.snapshot.id.0,
                    activation.effective_model_ref()
                )
            })
    }
}
