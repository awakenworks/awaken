// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: This source implements the repository-controlled permission product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! First-boot operator credential hand-off for embedded IAM.

use std::path::Path;

use super::{ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, ManagementAuthz, TokenSpec};

/// Mint the first `admin` token and hand it off through an owner-only file.
pub(super) fn bootstrap_admin_token(
    authz: &ManagementAuthz,
    dir: &Path,
    workspace_id: &str,
) -> Result<(), String> {
    let path = dir.join(ADMIN_TOKEN_FILE);
    authz
        .mint_token_with_handoff(
            TokenSpec {
                token_id: "tok_mgmt_bootstrap".to_string(),
                service_id: BOOTSTRAP_PRINCIPAL.to_string(),
                workspace_id: workspace_id.to_string(),
                role: "admin".to_string(),
                created_at: None,
                expires_at: None,
            },
            Some(&path),
        )
        .map_err(|error| error.to_string())?;
    eprintln!("{}", bootstrap_notice(&path, workspace_id));
    Ok(())
}

fn bootstrap_notice(path: &Path, workspace_id: &str) -> String {
    format!(
        "awaken-coordinator: EMBEDDED IAM BOOTSTRAP — minted the admin API token \
         for principal `{BOOTSTRAP_PRINCIPAL}` in workspace `{workspace_id}`.\n\
         Its cleartext is written only to {} (mode 0600), never to logs.\n\
         ROTATE IT: anyone who can read that file has full management authority.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_notice_is_secret_free() {
        let notice = bootstrap_notice(Path::new("/run/awaken/admin-token"), "workspace-a");
        assert!(notice.contains("/run/awaken/admin-token"));
        assert!(notice.contains("workspace-a"));
        assert!(!notice.contains("sk-awaken-"));
    }
}
