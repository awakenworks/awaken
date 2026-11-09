// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Internal request markers only select the already verified native
// @formal reason: identity/fence; HTTP input cannot create an admission or a grant.
//! Transient native-admission coordinates consumed by the original IAM guard.
/// Produced by the fine definition edge; never decoded from an HTTP field.
#[derive(Clone)]
pub(crate) struct AdmittedDefinition;
/// A transient, private current-native admission coordinate, not a grant or
/// browser-supplied identity. It selects single-attempt fenced runtime PDP for
/// this newly governed path; the legacy default transport remains unchanged.
#[derive(Clone)]
pub(crate) struct CurrentManagedDefinitionRequest {
    principal: awaken_iam_contract::PrincipalRef,
    workspace: String,
    fence: String,
}
impl CurrentManagedDefinitionRequest {
    pub(crate) fn new(
        principal: awaken_iam_contract::PrincipalRef,
        workspace: String,
        fence: String,
    ) -> Self {
        Self {
            principal,
            workspace,
            fence,
        }
    }
    pub(crate) fn belongs_to(
        &self,
        principal: &awaken_iam_contract::PrincipalRef,
        workspace: &str,
    ) -> bool {
        &self.principal == principal && self.workspace == workspace
    }
    pub(crate) fn fence(&self) -> &str {
        &self.fence
    }
}
