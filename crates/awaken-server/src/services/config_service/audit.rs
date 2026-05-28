use awaken_server_contract::AuditAction;
use axum::http::HeaderMap;
use serde_json::Value;

use super::{ConfigNamespace, ConfigService};

impl ConfigService {
    /// Emit an audit event if an audit logger is configured.
    ///
    /// Best-effort: the call is fire-and-forget (matching the `AuditLogger::emit` contract).
    pub(super) async fn emit_audit(
        &self,
        action: AuditAction,
        namespace: ConfigNamespace,
        id: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        self.emit_audit_with_suffix(action, namespace, id, "", before, after, headers)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_audit_with_suffix(
        &self,
        action: AuditAction,
        namespace: ConfigNamespace,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        self.emit_audit_with_suffix_in_namespace(
            action,
            namespace.as_str(),
            id,
            suffix,
            before,
            after,
            headers,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_audit_with_suffix_in_namespace(
        &self,
        action: AuditAction,
        namespace: &str,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let resource = if suffix.is_empty() {
            format!("{namespace}/{id}")
        } else {
            format!("{namespace}/{id}/{suffix}")
        };
        audit.emit(action, &resource, before, after, headers).await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_audit_apply_failed(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        error_msg: String,
        headers: &HeaderMap,
    ) {
        self.emit_audit_apply_failed_in_namespace(
            namespace.as_str(),
            id,
            suffix,
            before,
            after,
            error_msg,
            headers,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_audit_apply_failed_in_namespace(
        &self,
        namespace: &str,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        error_msg: String,
        headers: &HeaderMap,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let resource = if suffix.is_empty() {
            format!("{namespace}/{id}")
        } else {
            format!("{namespace}/{id}/{suffix}")
        };
        audit
            .emit_apply_failed(&resource, before, after, error_msg, headers)
            .await;
    }
}
