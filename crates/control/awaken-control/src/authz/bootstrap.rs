//! First-boot operator credential hand-off for embedded IAM.

use std::io::Write;
use std::path::Path;

use super::{ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, ManagementAuthz, TokenSpec};

/// Mint the first `admin` token, print it once, and persist it owner-only.
pub(super) fn bootstrap_admin_token(authz: &ManagementAuthz, dir: &Path, workspace_id: &str) {
    let secret = authz
        .mint_service_token(TokenSpec {
            token_id: "tok_mgmt_bootstrap".to_string(),
            service_id: BOOTSTRAP_PRINCIPAL.to_string(),
            workspace_id: workspace_id.to_string(),
            role: "admin".to_string(),
            created_at: None,
            expires_at: None,
        })
        .expect("mint the bootstrap admin token");
    let path = dir.join(ADMIN_TOKEN_FILE);
    write_owner_only(&path, &secret).expect("write the bootstrap admin-token file");
    eprintln!(
        "awaken-coordinator: EMBEDDED IAM BOOTSTRAP — minted the admin API token \
         for principal `{BOOTSTRAP_PRINCIPAL}` in workspace `{workspace_id}`.\n\
         It is printed ONCE and written to {} (mode 0600).\n\
         ROTATE IT: anyone holding this token has full management authority.\n\
         {secret}",
        path.display()
    );
}

/// Write `contents` to `path` readable by the owner only (0600 on unix).
fn write_owner_only(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}
