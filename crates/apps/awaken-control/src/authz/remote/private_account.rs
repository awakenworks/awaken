// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Uses native IAM current direct Account admission before selecting private content.
//! Use the original native IAM client for current human admission.

use awaken_iam_contract::AccountId;

use super::{AuthReject, RemoteManagementAuthz, unix_seconds_at};

impl RemoteManagementAuthz {
    pub(in crate::authz) fn current_direct_account(
        &self,
        token: &str,
    ) -> Result<AccountId, AuthReject> {
        let gate = self
            .gate
            .read()
            .map_err(|_| AuthReject::Unavailable)?
            .gate
            .clone();
        gate.authenticate_current_account(token, unix_seconds_at(self.clock.as_ref()))
    }
}
